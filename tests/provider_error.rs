use std::collections::HashMap;
use std::fs;

use serde_json::json;

mod common;
use common::{codemason, temp_dir, RoutedStubServer, StubResponse};

fn write_models_toml(cwd: &std::path::Path, model_id: &str) {
    let content = format!(
        r#"
[[model]]
id = "{model_id}"
role = "primary"

[gating]
min_context_length = 8000
require_tool_support = true
allow_unlisted = false
"#
    );
    fs::write(cwd.join("models.toml"), content).unwrap();
}

/// AC2, CLI level: five consecutive 500s from the provider exhaust the
/// retry budget and the whole `codemason run` process exits 5 — not just
/// the `llm::Client` call returning a `ProviderExhausted` error (covered at
/// the library level in tests/llm_client.rs).
#[test]
fn five_consecutive_500s_exit_the_process_with_code_5() {
    let cwd = temp_dir("provider-error-500x5");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/flaky-model";
    write_models_toml(&cwd, model_id);

    let mut routes = HashMap::new();
    routes.insert(
        "/models",
        vec![StubResponse::json(
            200,
            json!({"data": [{"id": model_id, "context_length": 32000, "supported_parameters": ["tools"]}]})
                .to_string(),
        )],
    );
    routes.insert(
        "/chat/completions",
        (0..5)
            .map(|_| StubResponse::json(500, r#"{"error":"boom"}"#))
            .collect(),
    );
    let stub = RoutedStubServer::start(routes);

    let output = codemason(&cwd)
        .args([
            "run",
            "--repo",
            cwd.to_str().unwrap(),
            "--task",
            "hello",
            "--model",
            model_id,
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", cwd.join("cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(
        output.status.code(),
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let completions_requests = stub
        .requests()
        .into_iter()
        .filter(|r| r.contains("/chat/completions"))
        .count();
    assert_eq!(completions_requests, 5, "expected exactly five retry attempts");
}
