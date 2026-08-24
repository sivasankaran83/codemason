//! Wire types for the OpenAI-compatible chat completions endpoint.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FunctionSpec,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Tolerant of an absent or malformed usage object — every field defaults
/// to zero/`None` rather than failing to parse.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Only ever set from a value the provider actually reported — never
    /// estimated locally.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Prompt-cache detail, when the provider reports it.
    ///
    /// The conversation prefix — system prompt, then task, then an
    /// append-only history — never changes between calls, which is what lets
    /// a provider cache it. Most of a run's prompt spend is that repeated
    /// prefix, so how much of it was served from cache is the single most
    /// useful cost number after the totals themselves. Measured, never
    /// inferred: absent when the provider says nothing.
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// OpenRouter reports the cache saving directly, as a negative-cost
    /// adjustment already reflected in `cost`.
    #[serde(default)]
    pub cache_discount: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

impl Usage {
    /// Prompt tokens served from the provider's cache, when reported.
    pub fn cached_tokens(&self) -> u64 {
        self.prompt_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct UsageInclude {
    pub include: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct CompletionRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub tools: &'a [ToolDef],
    pub tool_choice: &'static str,
    pub usage: UsageInclude,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CompletionResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    pub message: ChatMessage,
}
