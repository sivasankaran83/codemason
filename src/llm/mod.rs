//! Blocking OpenAI-compatible chat completions client. Retries `429`/`5xx`
//! with exponential backoff and jitter; anything else is a single-shot
//! failure. Never estimates a cost the provider didn't report.

pub mod cache;
mod types;

use std::collections::HashMap;
use std::time::Duration;

use rand::Rng;

pub use types::{ChatMessage, FunctionCall, FunctionSpec, ToolCall, ToolDef, Usage};

use crate::error::Error;

const MAX_ATTEMPTS: u32 = 5;
const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);

pub struct Client {
    base_url: String,
    api_key: String,
    http: ureq::Agent,
    base_delay: Duration,
}

#[derive(Debug)]
pub struct CompletionResult {
    pub message: ChatMessage,
    pub usage: Option<Usage>,
}

impl Client {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            http: ureq::AgentBuilder::new().build(),
            base_delay: DEFAULT_BASE_DELAY,
        }
    }

    /// Overrides the backoff base delay (production default: 1s, cap 60s,
    /// derived as `base * 60` so the cap scales with it). Used by tests to
    /// avoid real multi-second sleeps.
    pub fn with_backoff_base(mut self, base_delay: Duration) -> Self {
        self.base_delay = base_delay;
        self
    }

    pub fn complete(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDef],
    ) -> Result<CompletionResult, Error> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        // Serialising through `Value` first produces byte-identical JSON to
        // the typed path; it exists so cache breakpoints can be applied to
        // the messages that need them, for the models that need them. A model
        // that caches on its own is sent exactly what it was sent before.
        let wire: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null))
            .collect();
        let wire = if cache::needs_explicit_breakpoints(model) {
            cache::apply_breakpoints(wire)
        } else {
            wire
        };

        let body = types::CompletionRequest {
            model,
            messages: wire,
            tools,
            tool_choice: "auto",
            usage: types::UsageInclude { include: true },
        };

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let request = self
                .http
                .post(&url)
                .set("Authorization", &format!("Bearer {}", self.api_key))
                .set("Content-Type", "application/json");

            match request.send_json(&body) {
                Ok(response) => {
                    let parsed: types::CompletionResponse =
                        response.into_json().map_err(|source| Error::ProviderRequest {
                            model: model.to_string(),
                            source: source.into(),
                        })?;
                    let message = parsed
                        .choices
                        .into_iter()
                        .next()
                        .map(|c| c.message)
                        .ok_or_else(|| Error::ProviderRequest {
                            model: model.to_string(),
                            source: anyhow::anyhow!("response had no choices"),
                        })?;
                    return Ok(CompletionResult {
                        message,
                        usage: parsed.usage,
                    });
                }
                Err(ureq::Error::Status(code, response)) => {
                    let retryable = code == 429 || (500..600).contains(&code);
                    if !retryable {
                        let body_text = response.into_string().unwrap_or_default();
                        return Err(Error::ProviderRequest {
                            model: model.to_string(),
                            source: anyhow::anyhow!("status {code}: {body_text}"),
                        });
                    }
                    if attempt >= MAX_ATTEMPTS {
                        return Err(Error::ProviderExhausted {
                            model: model.to_string(),
                            attempts: attempt,
                            last_status: code,
                        });
                    }
                    self.sleep_with_backoff(attempt);
                }
                Err(ureq::Error::Transport(transport)) => {
                    return Err(Error::ProviderRequest {
                        model: model.to_string(),
                        source: transport.into(),
                    });
                }
            }
        }
    }

    fn sleep_with_backoff(&self, attempt: u32) {
        let cap = self.base_delay * 60;
        let exp = self.base_delay.saturating_mul(1u32 << (attempt.saturating_sub(1)).min(20));
        let delay = exp.min(cap);
        let jitter_ms = rand::rng().random_range(0..=250u64);
        std::thread::sleep(delay + Duration::from_millis(jitter_ms));
    }
}

/// Accumulated prompt/completion/total tokens and cost, one entry per model
/// id. Cost only ever grows from a value the provider actually reported.
#[derive(Debug, Clone, Default)]
pub struct Totals {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
    pub usage_missing_count: u64,
}

#[derive(Debug, Default)]
pub struct UsageLedger {
    by_model: HashMap<String, Totals>,
}

impl UsageLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, model: &str, usage: Option<&Usage>) {
        let entry = self.by_model.entry(model.to_string()).or_default();
        match usage {
            Some(u) => {
                entry.prompt_tokens += u.prompt_tokens;
                entry.completion_tokens += u.completion_tokens;
                entry.total_tokens += u.total_tokens;
                if let Some(cost) = u.cost {
                    entry.cost += cost;
                }
            }
            None => entry.usage_missing_count += 1,
        }
    }

    pub fn totals(&self) -> &HashMap<String, Totals> {
        &self.by_model
    }

    /// Cumulative tokens spent across every model — what the budget check
    /// compares `LoopConfig::budget_tokens` against.
    pub fn total_tokens(&self) -> u64 {
        self.by_model.values().map(|t| t.total_tokens).sum()
    }

    /// Cumulative cost across every model, from whatever the provider
    /// actually reported — never estimated.
    pub fn total_cost(&self) -> f64 {
        self.by_model.values().map(|t| t.cost).sum()
    }
}
