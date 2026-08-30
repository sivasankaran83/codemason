//! Eliding stale tool results from the conversation before a call.
//!
//! ## Why this exists, and why it is not summarisation
//!
//! Conversation history is append-only and re-sent whole on every call, so a
//! run's prompt spend grows quadratically with its iteration count. Measured
//! on a real 21-iteration run: 411,469 prompt tokens billed for a
//! conversation whose final size was 33,678 — 92% of the spend was re-sending
//! history.
//!
//! That looks like an argument for sending less. It is not, and this module
//! is **off by default** as a result. Re-sent history is the provider's cache
//! prefix, and cached tokens cost about a tenth of fresh ones; eliding
//! rewrites old messages, changes that prefix, and forfeits the discount.
//! Measured against a real provider on an identical task, eliding sent 21%
//! fewer tokens and cost 24% more. See [`DEFAULT_KEEP_RECENT_TURNS`].
//!
//! What remains true is that elision shrinks the *context window*, which the
//! cache does not. That is the case this exists for.
//!
//! SPEC.md's Must Not list forbids summarising or truncating conversation
//! context, and that rule is right about the thing it was aimed at. A
//! summarised conversation loses what the agent decided and why, and the
//! failure mode — an agent that silently forgets a constraint it agreed to
//! four turns ago — is close to undebuggable.
//!
//! So this does something narrower and recoverable:
//!
//! - **Every assistant message is kept, in full, forever.** The decision
//!   chain is exactly what must not be lost.
//! - **The system prompt and the task are kept, in full, forever.** They are
//!   also the cache prefix, so touching them would cost more than it saves.
//! - **Only `tool` results are elided, and only old ones.** A file read from
//!   iteration three has already been acted on; its content is the bulk of
//!   the context and the most disposable part of it.
//! - **Elision is announced and reversible.** The placeholder names the tool
//!   and the size, and tells the model it can run the tool again. An agent
//!   that still needs the content can go and get it, which is the difference
//!   between eliding and forgetting.
//!
//! See SPEC.md, "Amendment: eliding stale tool results".

use crate::llm::ChatMessage;

/// Elision is **off by default**, on measurement.
///
/// The obvious reasoning — history is 92% of prompt spend, so send less of
/// it — is wrong, because it counts bytes rather than money. Eliding rewrites
/// old tool results, which changes the conversation prefix on every call, and
/// a changed prefix cannot be served from the provider's prompt cache. Cached
/// tokens cost roughly a tenth of fresh ones, so trading the cache for a
/// smaller payload trades a ~90% discount for a ~20% one.
///
/// Measured on an identical read-heavy task against a real provider:
///
/// | | prompt tokens | cached | cost |
/// |---|---|---|---|
/// | elision off | 59,972 | 35,825 (59.7%) | **$0.0124** |
/// | elision on (keep 3) | 47,252 | 0 (0%) | $0.0154 |
///
/// Fewer tokens, more money. An earlier A/B appeared to show a 42% saving,
/// but it ran against a stub with no caching and measured bytes on the wire
/// rather than what they cost — the wrong quantity.
///
/// So this exists for the case where the **context window**, not the bill, is
/// the binding constraint: a conversation that would otherwise overflow. Then
/// something must go, and a recoverable placeholder beats a hard failure. Set
/// `--keep-recent-turns 3` deliberately for that; do not set it to save money.
pub const DEFAULT_KEEP_RECENT_TURNS: u32 = 0;

/// Below this, eliding costs more in explanation than it saves in tokens.
const MIN_ELIDE_CHARS: usize = 400;

fn placeholder(tool: Option<&str>, bytes: usize) -> String {
    let name = tool.unwrap_or("tool");
    format!(
        "[elided: {bytes} bytes of earlier `{name}` output, removed to keep this \
         conversation affordable. Nothing was summarised or altered. If you still \
         need it, call the tool again.]"
    )
}

/// Return a copy of `history` with stale tool results replaced by
/// placeholders.
///
/// `keep_recent_turns` counts assistant messages from the end; tool results
/// belonging to that window are untouched. `0` disables elision entirely and
/// returns the history unchanged.
pub fn elide_stale_tool_results(
    history: &[ChatMessage],
    keep_recent_turns: u32,
) -> (Vec<ChatMessage>, ElisionStats) {
    let mut stats = ElisionStats::default();
    if keep_recent_turns == 0 {
        return (history.to_vec(), stats);
    }

    // Index of the assistant message that opens the protected window.
    let assistant_positions: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "assistant")
        .map(|(i, _)| i)
        .collect();

    let cutoff = if assistant_positions.len() as u32 <= keep_recent_turns {
        // Not enough turns to have anything stale yet.
        return (history.to_vec(), stats);
    } else {
        assistant_positions[assistant_positions.len() - keep_recent_turns as usize]
    };

    let out = history
        .iter()
        .enumerate()
        .map(|(i, msg)| {
            if i >= cutoff || msg.role != "tool" {
                return msg.clone();
            }
            let Some(content) = msg.content.as_ref() else {
                return msg.clone();
            };
            if content.len() < MIN_ELIDE_CHARS || content.starts_with("[elided:") {
                return msg.clone();
            }

            stats.elided_messages += 1;
            stats.elided_bytes += content.len();

            let mut elided = msg.clone();
            elided.content = Some(placeholder(
                tool_name_for(history, msg.tool_call_id.as_deref()),
                content.len(),
            ));
            elided
        })
        .collect();

    (out, stats)
}

/// Resolve which tool produced a result, by matching its `tool_call_id`
/// against the assistant message that requested it.
///
/// Read out of the history rather than stored on the tool message, because
/// the `name` field on a `tool` message is a legacy of the old `function`
/// role and not every provider tolerates it. Nothing here changes what goes
/// on the wire.
fn tool_name_for<'a>(history: &'a [ChatMessage], tool_call_id: Option<&str>) -> Option<&'a str> {
    let id = tool_call_id?;
    history
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .find(|call| call.id == id)
        .map(|call| call.function.name.as_str())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ElisionStats {
    pub elided_messages: usize,
    pub elided_bytes: usize,
}

impl ElisionStats {
    pub fn any(&self) -> bool {
        self.elided_messages > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(n: usize) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: Some(format!("thinking {n}")),
            ..Default::default()
        }
    }

    fn tool_result(id: &str, body: &str) -> ChatMessage {
        ChatMessage::tool_result(id.to_string(), body.to_string())
    }

    /// An assistant turn that requested `read_file`, so the elision
    /// placeholder can name the tool that produced the result.
    fn assistant_calling(n: usize, call_id: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: Some(format!("thinking {n}")),
            tool_calls: Some(vec![crate::llm::ToolCall {
                id: call_id.to_string(),
                kind: "function".to_string(),
                function: crate::llm::FunctionCall {
                    name: "read_file".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            ..Default::default()
        }
    }

    fn big(n: usize) -> String {
        "x".repeat(n)
    }

    fn history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("sys"),
            ChatMessage::user("task"),
            assistant_calling(1, "t1"),
            tool_result("t1", &big(5000)),
            assistant_calling(2, "t2"),
            tool_result("t2", &big(5000)),
            assistant_calling(3, "t3"),
            tool_result("t3", &big(5000)),
            assistant_calling(4, "t4"),
            tool_result("t4", &big(5000)),
        ]
    }

    #[test]
    fn zero_disables_elision_entirely() {
        let h = history();
        let (out, stats) = elide_stale_tool_results(&h, 0);
        assert!(!stats.any());
        assert_eq!(out.len(), h.len());
        for (a, b) in out.iter().zip(h.iter()) {
            assert_eq!(a.content, b.content);
        }
    }

    #[test]
    fn the_system_prompt_and_task_are_never_touched() {
        let h = history();
        let (out, _) = elide_stale_tool_results(&h, 1);
        assert_eq!(out[0].content.as_deref(), Some("sys"));
        assert_eq!(out[1].content.as_deref(), Some("task"));
    }

    #[test]
    fn every_assistant_message_survives_in_full() {
        let h = history();
        let (out, _) = elide_stale_tool_results(&h, 1);
        let kept: Vec<_> = out
            .iter()
            .filter(|m| m.role == "assistant")
            .filter_map(|m| m.content.clone())
            .collect();
        assert_eq!(kept, vec!["thinking 1", "thinking 2", "thinking 3", "thinking 4"]);
    }

    #[test]
    fn recent_tool_results_are_kept_and_older_ones_elided() {
        let h = history();
        let (out, stats) = elide_stale_tool_results(&h, 2);

        // Window opens at assistant(3), so t1 and t2 are stale; t3 and t4 stay.
        assert_eq!(stats.elided_messages, 2);
        assert_eq!(stats.elided_bytes, 10_000);

        let tools: Vec<&ChatMessage> = out.iter().filter(|m| m.role == "tool").collect();
        assert!(tools[0].content.as_deref().unwrap().starts_with("[elided:"));
        assert!(tools[1].content.as_deref().unwrap().starts_with("[elided:"));
        assert_eq!(tools[2].content.as_deref().unwrap().len(), 5000);
        assert_eq!(tools[3].content.as_deref().unwrap().len(), 5000);
    }

    #[test]
    fn the_placeholder_tells_the_model_how_to_recover_the_content() {
        let h = history();
        let (out, _) = elide_stale_tool_results(&h, 1);
        let first = out
            .iter()
            .find(|m| m.role == "tool")
            .and_then(|m| m.content.clone())
            .unwrap();
        assert!(first.contains("read_file"), "names the tool: {first}");
        assert!(first.contains("5000"), "states the size: {first}");
        assert!(first.contains("call the tool again"), "says how to recover: {first}");
    }

    #[test]
    fn a_short_result_is_not_worth_eliding() {
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("task"),
            assistant(1),
            tool_result("t1", "ok"),
            assistant(2),
            assistant(3),
            assistant(4),
        ];
        let (out, stats) = elide_stale_tool_results(&h, 1);
        assert!(!stats.any());
        assert_eq!(
            out.iter().find(|m| m.role == "tool").unwrap().content.as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn eliding_twice_does_not_re_elide_a_placeholder() {
        let h = history();
        let (once, s1) = elide_stale_tool_results(&h, 1);
        let (twice, s2) = elide_stale_tool_results(&once, 1);
        assert!(s1.any());
        assert!(!s2.any(), "a placeholder must not be elided again");
        assert_eq!(once.len(), twice.len());
    }

    #[test]
    fn a_short_conversation_is_left_alone() {
        let h = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("task"),
            assistant(1),
            tool_result("t1", &big(5000)),
        ];
        let (_, stats) = elide_stale_tool_results(&h, DEFAULT_KEEP_RECENT_TURNS);
        assert!(!stats.any(), "nothing is stale yet");
    }
}
