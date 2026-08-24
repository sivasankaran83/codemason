use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Duration;

use serde_json::{json, Value};

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

fn catalogue_body(model_id: &str) -> String {
    json!({"data": [{"id": model_id, "context_length": 32000, "supported_parameters": ["tools"]}]}).to_string()
}

fn tool_call_body(id: &str) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": "context_search", "arguments": "{\"query\":\"main\"}"}
                }]
            }
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
    })
    .to_string()
}

fn summary_body(text: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
    })
    .to_string()
}

/// AC9 (a): a completed run's log parses line by line with contiguous
/// `seq`, and contains `run_started`, `index_built`, at least one
/// `llm_call`, and `run_completed`.
#[test]
fn ac9a_completed_run_log_has_contiguous_seq_and_required_types() {
    let cwd = temp_dir("event-log-full-run");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/log-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(200, tool_call_body("1")),
            StubResponse::json(200, summary_body("done")),
        ],
    );
    let stub = RoutedStubServer::start(routes);
    let log_path = cwd.join("run.jsonl");

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
            "--log",
            log_path.to_str().unwrap(),
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("event-log-full-run-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = fs::read_to_string(&log_path).expect("log file should exist");
    let lines: Vec<&str> = content.lines().collect();
    assert!(!lines.is_empty());

    let mut seqs = Vec::new();
    let mut types = HashSet::new();
    for line in &lines {
        let parsed: Value = serde_json::from_str(line).expect("valid JSON line");
        seqs.push(parsed["seq"].as_u64().unwrap());
        types.insert(parsed["type"].as_str().unwrap().to_string());
    }
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "seq must be contiguous starting at 1");

    for required in ["run_started", "index_built", "llm_call", "run_completed"] {
        assert!(
            types.contains(required),
            "missing event type {required:?}, saw {types:?}"
        );
    }
}

/// AC9 (b): killing mid-run leaves every complete line parseable.
#[test]
fn ac9b_killing_mid_run_leaves_complete_lines_parseable() {
    let cwd = temp_dir("event-log-kill");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/kill-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    // A long run of identical tool-call turns, each delayed, so the process
    // is still going when the test kills it.
    let many_tool_calls: Vec<StubResponse> = (0..50)
        .map(|i| StubResponse::json(200, tool_call_body(&i.to_string())))
        .collect();
    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert("/chat/completions", many_tool_calls);
    let stub = RoutedStubServer::start_with_delay(routes, Duration::from_millis(100));
    let log_path = cwd.join("run.jsonl");

    let mut child = codemason(&cwd)
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
            "--log",
            log_path.to_str().unwrap(),
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("event-log-kill-cache"))
        .spawn()
        .expect("spawn codemason run");

    std::thread::sleep(Duration::from_millis(600));
    let _ = child.kill();
    let _ = child.wait();

    let content = fs::read_to_string(&log_path).expect("log file should exist");
    let lines: Vec<&str> = content.lines().collect();
    assert!(
        lines.len() >= 2,
        "expected several lines to have been written before the kill, got {}",
        lines.len()
    );

    // Every line except possibly a truncated final one must parse.
    for line in &lines[..lines.len() - 1] {
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("non-final line failed to parse: {e}\nline: {line}"));
    }
}
