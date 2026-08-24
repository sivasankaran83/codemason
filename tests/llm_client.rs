use std::collections::HashMap;
use std::time::Duration;

use codemason_core::llm::{ChatMessage, Client};
use codemason_core::Error;
use serde_json::json;

mod common;
use common::{RoutedStubServer, StubResponse};

fn assistant_response(content: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    })
    .to_string()
}

/// AC1: a debug round trip returns a parsed message and non-zero token
/// counts.
#[test]
fn ac1_debug_round_trip_returns_parsed_message_and_nonzero_tokens() {
    let mut routes = HashMap::new();
    routes.insert("/chat/completions", vec![StubResponse::json(200, assistant_response("hello there"))]);
    let stub = RoutedStubServer::start(routes);

    let client = Client::new(stub.base_url.clone(), "test-key".to_string());
    let messages = vec![ChatMessage::user("hi")];
    let result = client
        .complete("vendor/model", &messages, &[])
        .expect("completion should succeed");

    assert_eq!(result.message.content.as_deref(), Some("hello there"));
    let usage = result.usage.expect("usage should be present");
    assert!(usage.total_tokens > 0, "expected non-zero token counts");
}

/// AC2 (a): a stub returning 429, 429, 200 succeeds.
#[test]
fn ac2a_retries_429_then_succeeds() {
    let mut routes = HashMap::new();
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(429, r#"{"error":"rate limited"}"#),
            StubResponse::json(429, r#"{"error":"rate limited"}"#),
            StubResponse::json(200, assistant_response("done after retries")),
        ],
    );
    let stub = RoutedStubServer::start(routes);

    let client = Client::new(stub.base_url.clone(), "test-key".to_string())
        .with_backoff_base(Duration::from_millis(5));
    let messages = vec![ChatMessage::user("hi")];
    let result = client
        .complete("vendor/model", &messages, &[])
        .expect("should eventually succeed");

    assert_eq!(result.message.content.as_deref(), Some("done after retries"));
    assert_eq!(stub.requests().len(), 3, "expected exactly three attempts");
}

/// AC2 (b): 500 five times exits with an exhausted-retry error (the loop
/// maps this to exit 5).
#[test]
fn ac2b_exhausts_after_five_consecutive_500s() {
    let responses: Vec<StubResponse> = (0..5)
        .map(|_| StubResponse::json(500, r#"{"error":"boom"}"#))
        .collect();
    let mut routes = HashMap::new();
    routes.insert("/chat/completions", responses);
    let stub = RoutedStubServer::start(routes);

    let client = Client::new(stub.base_url.clone(), "test-key".to_string())
        .with_backoff_base(Duration::from_millis(5));
    let messages = vec![ChatMessage::user("hi")];
    let err = client
        .complete("vendor/model", &messages, &[])
        .expect_err("should exhaust retries");

    match err {
        Error::ProviderExhausted { attempts, last_status, .. } => {
            assert_eq!(attempts, 5);
            assert_eq!(last_status, 500);
        }
        other => panic!("expected ProviderExhausted, got {other:?}"),
    }
    assert_eq!(stub.requests().len(), 5);
}

/// AC2 (c): a response with usage removed does not panic.
#[test]
fn ac2c_missing_usage_does_not_panic() {
    let body = json!({
        "choices": [{"message": {"role": "assistant", "content": "no usage in this one"}}]
    })
    .to_string();
    let mut routes = HashMap::new();
    routes.insert("/chat/completions", vec![StubResponse::json(200, body)]);
    let stub = RoutedStubServer::start(routes);

    let client = Client::new(stub.base_url.clone(), "test-key".to_string());
    let messages = vec![ChatMessage::user("hi")];
    let result = client
        .complete("vendor/model", &messages, &[])
        .expect("should still succeed without usage");

    assert!(result.usage.is_none());
}
