//! The tool-calling loop. Seeds history with a system prompt and the task,
//! calls the model, executes any tool calls in order, appends one `tool`
//! message per call, and repeats until an assistant message carries no tool
//! calls — that message is the summary.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::json;

use crate::index::Index;
use crate::llm::{self, ChatMessage, UsageLedger};
use crate::log::{event_type, EventLog};
use crate::text::normalize_slashes;
use crate::tools::{self, DispatchResult, ToolContext};

/// Three consecutive same-tool argument-parse failures, or three
/// consecutive responses with no usable usage data, abort the run. Neither
/// retries the API call — SPEC.md T3.4.
const CONSECUTIVE_FAILURE_LIMIT: u32 = 3;

pub struct LoopConfig {
    pub repo_root: PathBuf,
    pub task: String,
    pub model: String,
    pub max_iterations: u32,
    pub budget_tokens: u64,
    pub budget_usd: Option<f64>,
    pub dry_run: bool,
}

#[derive(Debug)]
pub enum LoopExit {
    Completed { summary: String, iterations: u32 },
    ProviderError { reason: String, iterations: u32 },
    BudgetExceeded { iterations: u32 },
    MaxIterationsExceeded { iterations: u32 },
}

/// Checked immediately before each API call, never after — a call already
/// made has already been paid for. Strict-zero start refusal: a
/// `budget_tokens`/`budget_usd` of exactly zero (or negative) refuses before
/// the first call; otherwise the check compares tokens/cost already spent
/// against the configured cap.
fn budget_breached(cfg: &LoopConfig, ledger: &UsageLedger) -> bool {
    if cfg.budget_tokens == 0 {
        return true;
    }
    if let Some(cap) = cfg.budget_usd {
        if cap <= 0.0 || ledger.total_cost() >= cap {
            return true;
        }
    }
    ledger.total_tokens() >= cfg.budget_tokens
}

fn system_prompt(repo_root: &std::path::Path) -> String {
    format!(
        "You are operating on a git repository at {}. Start discovery with \
         context_search, then context_outline, then read_file. When you are \
         finished, reply with a summary and no tool calls.",
        normalize_slashes(&repo_root.to_string_lossy())
    )
}

pub fn run(
    cfg: &LoopConfig,
    client: &llm::Client,
    index: &Index,
    log: &mut EventLog,
) -> (LoopExit, UsageLedger) {
    let mut history = vec![
        ChatMessage::system(system_prompt(&cfg.repo_root)),
        ChatMessage::user(cfg.task.clone()),
    ];
    let tool_defs = tools::as_llm_tool_defs();
    let mut ledger = UsageLedger::new();

    let mut iterations: u32 = 0;
    let mut consecutive_missing_usage: u32 = 0;
    let mut parse_failures: HashMap<String, u32> = HashMap::new();

    loop {
        if budget_breached(cfg, &ledger) {
            log.write(
                event_type::BUDGET_EXCEEDED,
                json!({
                    "iterations": iterations,
                    "tokens_used": ledger.total_tokens(),
                    "budget_tokens": cfg.budget_tokens,
                    "cost_used": ledger.total_cost(),
                    "budget_usd": cfg.budget_usd,
                }),
            );
            return (LoopExit::BudgetExceeded { iterations }, ledger);
        }
        if iterations >= cfg.max_iterations {
            log.write(
                event_type::MAX_ITERATIONS_EXCEEDED,
                json!({"iterations": iterations, "max_iterations": cfg.max_iterations}),
            );
            return (LoopExit::MaxIterationsExceeded { iterations }, ledger);
        }
        iterations += 1;

        let completion = match client.complete(&cfg.model, &history, &tool_defs) {
            Ok(result) => result,
            Err(err) => {
                log.write(
                    event_type::RUN_FAILED,
                    json!({"reason": err.to_string(), "iterations": iterations}),
                );
                return (
                    LoopExit::ProviderError {
                        reason: err.to_string(),
                        iterations,
                    },
                    ledger,
                );
            }
        };

        ledger.record(&cfg.model, completion.usage.as_ref());

        match &completion.usage {
            Some(usage) => {
                consecutive_missing_usage = 0;
                log.write(
                    event_type::LLM_CALL,
                    json!({
                        "model": cfg.model,
                        "iteration": iterations,
                        "prompt_tokens": usage.prompt_tokens,
                        "completion_tokens": usage.completion_tokens,
                        "total_tokens": usage.total_tokens,
                    }),
                );
            }
            None => {
                consecutive_missing_usage += 1;
                log.write(
                    event_type::LLM_CALL,
                    json!({"model": cfg.model, "iteration": iterations, "usage": Option::<u64>::None}),
                );
                log.write(
                    event_type::USAGE_MISSING,
                    json!({"model": cfg.model, "consecutive_count": consecutive_missing_usage}),
                );
                if consecutive_missing_usage >= CONSECUTIVE_FAILURE_LIMIT {
                    let reason = format!(
                        "{consecutive_missing_usage} consecutive responses without usage data"
                    );
                    log.write(
                        event_type::RUN_FAILED,
                        json!({"reason": reason, "iterations": iterations}),
                    );
                    return (LoopExit::ProviderError { reason, iterations }, ledger);
                }
            }
        }

        let assistant_message = completion.message;
        let has_tool_calls = assistant_message.has_tool_calls();
        history.push(assistant_message.clone());

        if !has_tool_calls {
            let summary = assistant_message.content.unwrap_or_default();
            log.write(
                event_type::RUN_COMPLETED,
                json!({"iterations": iterations}),
            );
            return (LoopExit::Completed { summary, iterations }, ledger);
        }

        let calls = assistant_message.tool_calls.unwrap_or_default();
        for call in calls {
            let tool_name = call.function.name.clone();
            log.write(
                event_type::TOOL_CALL,
                json!({"tool": tool_name, "iteration": iterations}),
            );

            let ctx = ToolContext {
                repo_root: &cfg.repo_root,
                index,
                dry_run: cfg.dry_run,
            };

            match tools::dispatch(&tool_name, &call.function.arguments, &ctx) {
                DispatchResult::Ran(outcome) => {
                    parse_failures.remove(&tool_name);
                    let ok = matches!(outcome, tools::ToolOutcome::Ok(_));
                    let text = outcome.into_text();
                    log.write(
                        event_type::TOOL_RESULT,
                        json!({"tool": tool_name, "ok": ok, "result_chars": text.len(), "truncated": false}),
                    );
                    history.push(ChatMessage::tool_result(call.id.clone(), text));
                }
                DispatchResult::UnknownTool => {
                    let valid = tools::valid_names().join(", ");
                    let text = format!("unknown tool {tool_name:?}; valid tools are: {valid}");
                    log.write(
                        event_type::TOOL_RESULT,
                        json!({"tool": tool_name, "ok": false, "result_chars": text.len(), "truncated": false}),
                    );
                    history.push(ChatMessage::tool_result(call.id.clone(), text));
                }
                DispatchResult::BadArguments(reason) => {
                    let count = parse_failures.entry(tool_name.clone()).or_insert(0);
                    *count += 1;
                    let text = format!("arguments for {tool_name:?} failed to parse: {reason}");
                    log.write(
                        event_type::TOOL_RESULT,
                        json!({"tool": tool_name, "ok": false, "result_chars": text.len(), "truncated": false}),
                    );
                    history.push(ChatMessage::tool_result(call.id.clone(), text));

                    if *count >= CONSECUTIVE_FAILURE_LIMIT {
                        let reason = format!(
                            "{count} consecutive argument-parse failures for tool {tool_name:?}"
                        );
                        log.write(
                            event_type::RUN_FAILED,
                            json!({"reason": reason, "iterations": iterations}),
                        );
                        return (LoopExit::ProviderError { reason, iterations }, ledger);
                    }
                }
            }
        }
    }
}
