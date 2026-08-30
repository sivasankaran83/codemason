//! Explicit prompt-cache breakpoints.
//!
//! Most providers cache a repeated prompt prefix automatically. Some do not:
//! Anthropic, Alibaba Qwen and Gemini's explicit mode want `cache_control`
//! markers on individual content blocks, and without them they recompute the
//! whole prompt every turn.
//!
//! That difference is expensive, and it was measured rather than assumed. The
//! same read-heavy task, run twice through this binary before any of this
//! existed:
//!
//! | model | prompt tokens | cached | cost |
//! |---|---|---|---|
//! | `minimax/minimax-m2.7` (auto) | 59,972 | 35,825 (59.7%) | $0.0124 |
//! | `anthropic/claude-sonnet-5` | 47,385 | **0 (0%)** | $0.1029 |
//!
//! Fewer tokens, eight times the cost. A binary whose whole premise is vendor
//! independence should not be quietly worse on one vendor because of a wire
//! detail, which is what this module fixes.
//!
//! Measured again with breakpoints applied, same task, same iteration count:
//!
//! | | prompt tokens | cached | cost |
//! |---|---|---|---|
//! | `anthropic/claude-sonnet-5` before | 47,385 | 0 (0%) | $0.1029 |
//! | `anthropic/claude-sonnet-5` after | 47,375 | 27,173 (57.4%) | **$0.0692** |
//!
//! A third off, and the hit rate now matches what the automatic providers
//! reach on their own.
//!
//! ## Why the shape is delicate
//!
//! A breakpoint requires the message's `content` to become an array of typed
//! blocks rather than a plain string. Not every OpenAI-compatible endpoint
//! accepts that, so the array form is emitted **only** for the messages that
//! carry a breakpoint, and only for models known to need it. Everything else
//! goes on the wire exactly as it did before.

use serde_json::{json, Value};

/// Anthropic allows four; the others are looser. Four is the safe ceiling.
const MAX_BREAKPOINTS: usize = 4;

/// Whether a model needs explicit breakpoints to cache at all.
///
/// Matched on the id prefix because that is what an OpenAI-compatible
/// catalogue gives us. Unknown models are treated as caching automatically:
/// the cost of a missing breakpoint is a larger bill, while the cost of an
/// unexpected array-shaped `content` is a request the provider rejects
/// outright. Losing money beats not working.
pub fn needs_explicit_breakpoints(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("anthropic/")
        || m.starts_with("qwen/")
        || m.starts_with("alibaba/")
        || m.contains("claude-")
}

/// Attach `cache_control` to a message that serialises with string content.
///
/// Returns the message unchanged if it has no string content to convert —
/// a tool-call-only assistant turn, for instance.
fn mark(mut message: Value) -> Value {
    let Some(text) = message.get("content").and_then(|c| c.as_str()).map(str::to_owned) else {
        return message;
    };
    if text.is_empty() {
        return message;
    }
    message["content"] = json!([{
        "type": "text",
        "text": text,
        "cache_control": {"type": "ephemeral"},
    }]);
    message
}

/// Place cache breakpoints on an already-serialised message array.
///
/// Two breakpoints, which is the whole trick:
///
/// 1. **The end of the stable prefix** — the system prompt and the task never
///    change for the life of a run, so everything up to here is cacheable
///    from the second call onward.
/// 2. **The end of the conversation so far** — history is append-only, so
///    this call's whole payload becomes the *next* call's cached prefix. That
///    is what turns the re-sent history from the dominant cost into the
///    cheapest part of the request; on a measured run, 92% of prompt spend
///    was re-sent history, and cached reads are billed at roughly a tenth.
///
/// A breakpoint is only useful on a message whose content is stable text, so
/// assistant turns carrying tool calls are skipped and the marker moves to
/// the nearest earlier message that can hold one.
pub fn apply_breakpoints(mut messages: Vec<Value>) -> Vec<Value> {
    if messages.is_empty() {
        return messages;
    }

    let mut marked = 0usize;

    // 1. End of the stable prefix: the task, or the system prompt when the
    //    task has not been appended yet.
    let prefix_end = if messages.len() >= 2 { 1 } else { 0 };
    messages[prefix_end] = mark(messages[prefix_end].take());
    if messages[prefix_end].get("content").is_some_and(|c| c.is_array()) {
        marked += 1;
    }

    // 2. End of the conversation, so the next call finds this one cached.
    //    Walk back to the last message that can actually hold a marker.
    if messages.len() > prefix_end + 1 && marked < MAX_BREAKPOINTS {
        for i in (prefix_end + 1..messages.len()).rev() {
            let before = messages[i].clone();
            let after = mark(messages[i].take());
            let changed = after.get("content").is_some_and(|c| c.is_array());
            messages[i] = if changed { after } else { before };
            if changed {
                break;
            }
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    fn tool_call_turn() -> Value {
        json!({
            "role": "assistant",
            "tool_calls": [{"id": "c1", "type": "function",
                            "function": {"name": "read_file", "arguments": "{}"}}]
        })
    }

    fn breakpoints(messages: &[Value]) -> usize {
        messages
            .iter()
            .filter(|m| {
                m.get("content")
                    .and_then(|c| c.as_array())
                    .is_some_and(|a| a.iter().any(|b| b.get("cache_control").is_some()))
            })
            .count()
    }

    #[test]
    fn only_models_that_need_breakpoints_get_them() {
        assert!(needs_explicit_breakpoints("anthropic/claude-sonnet-5"));
        assert!(needs_explicit_breakpoints("qwen/qwen3-coder"));
        // These cache on their own; sending an array-shaped content to a
        // provider that does not expect it risks a rejected request for no
        // gain.
        assert!(!needs_explicit_breakpoints("minimax/minimax-m2.7"));
        assert!(!needs_explicit_breakpoints("deepseek/deepseek-v3.2"));
        assert!(!needs_explicit_breakpoints("openai/gpt-5.1-codex-mini"));
    }

    #[test]
    fn the_stable_prefix_and_the_tail_are_both_marked() {
        let out = apply_breakpoints(vec![
            msg("system", "sys"),
            msg("user", "task"),
            msg("assistant", "thinking"),
            msg("tool", "result"),
        ]);
        assert_eq!(breakpoints(&out), 2, "prefix and tail");
        // The system prompt itself stays a plain string: the prefix marker
        // sits on the task, and Anthropic caches everything up to a marker.
        assert!(out[0]["content"].is_string());
        assert!(out[1]["content"].is_array());
        assert!(out[3]["content"].is_array());
    }

    #[test]
    fn a_marker_never_lands_on_a_tool_call_turn() {
        // Such a turn has no string content to convert, so the marker must
        // fall back to the nearest message that can hold one.
        let out = apply_breakpoints(vec![
            msg("system", "sys"),
            msg("user", "task"),
            msg("tool", "earlier result"),
            tool_call_turn(),
        ]);
        assert!(out[3].get("content").is_none(), "tool-call turn untouched");
        assert!(out[2]["content"].is_array(), "marker moved back to the tool result");
        assert_eq!(breakpoints(&out), 2);
    }

    #[test]
    fn a_two_message_conversation_gets_one_marker_and_does_not_panic() {
        let out = apply_breakpoints(vec![msg("system", "sys"), msg("user", "task")]);
        assert_eq!(breakpoints(&out), 1);
    }

    #[test]
    fn an_empty_conversation_is_returned_unchanged() {
        assert!(apply_breakpoints(Vec::new()).is_empty());
    }

    #[test]
    fn the_marked_block_preserves_the_original_text_exactly() {
        let out = apply_breakpoints(vec![msg("system", "sys"), msg("user", "the task text")]);
        assert_eq!(out[1]["content"][0]["text"], "the task text");
        assert_eq!(out[1]["content"][0]["type"], "text");
        assert_eq!(out[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(out[1]["role"], "user", "role is untouched");
    }

    #[test]
    fn never_exceeds_the_providers_breakpoint_ceiling() {
        let mut convo = vec![msg("system", "sys"), msg("user", "task")];
        for i in 0..30 {
            convo.push(msg("assistant", &format!("turn {i}")));
            convo.push(msg("tool", &format!("result {i}")));
        }
        assert!(breakpoints(&apply_breakpoints(convo)) <= MAX_BREAKPOINTS);
    }
}
