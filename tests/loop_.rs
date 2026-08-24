use std::collections::HashMap;

use codemason_core::llm::Client;
use codemason_core::log::EventLog;
use codemason_core::{run_loop, Index, LoopConfig, LoopExit};
use serde_json::json;
use uuid::Uuid;

mod common;
use common::{RoutedStubServer, StubResponse};

fn temp_repo(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("codemason-loop-test-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.rs"), "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
    dir
}

fn tool_call_response(id: &str, tool: &str, args: serde_json::Value) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": tool, "arguments": args.to_string()}
                }]
            }
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
    })
    .to_string()
}

fn summary_response(text: &str) -> String {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
    })
    .to_string()
}

fn open_log(repo: &std::path::Path, label: &str) -> EventLog {
    let path = repo.join(".agent").join("log").join(format!("{label}.jsonl"));
    EventLog::open(&path, Uuid::now_v7()).expect("open log")
}

/// AC7 (mechanics half — see PLAN.md): a fabricated multi-turn trace proves
/// the loop calls `context_search` before `read_file` and terminates on a
/// no-tool-call message. This is not a substitute for AC7's live-model run,
/// only a check that the loop's own control flow is correct.
#[test]
fn ac7_loop_calls_context_search_before_read_file_and_terminates() {
    let repo = temp_repo("ac7");
    let mut routes = HashMap::new();
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(200, tool_call_response("1", "context_search", json!({"query": "main"}))),
            StubResponse::json(
                200,
                tool_call_response("2", "read_file", json!({"path": "a.rs", "start_line": 0, "end_line": 0})),
            ),
            StubResponse::json(200, summary_response("This repository has a single Rust file.")),
        ],
    );
    let stub = RoutedStubServer::start(routes);

    let index = Index::build(&repo).expect("index build should succeed");
    let client = Client::new(stub.base_url.clone(), "test-key".to_string());
    let mut log = open_log(&repo, "ac7");

    let cfg = LoopConfig {
        repo_root: repo.clone(),
        task: "Describe the structure of this repository".to_string(),
        model: "vendor/model".to_string(),
        max_iterations: 40,
        budget_tokens: 200_000,
        budget_usd: None,
        dry_run: false,
        keep_recent_turns: 0,
    };

    let (exit, _ledger) = run_loop(&cfg, &client, &index, &mut log);
    match exit {
        LoopExit::Completed { summary, iterations } => {
            assert_eq!(iterations, 3);
            assert!(summary.contains("Rust file"));
        }
        other => panic!("expected completion, got {other:?}"),
    }

    let requests = stub.requests();
    assert_eq!(requests.len(), 3, "expected exactly three chat completion calls");

    // The second request's history must already carry context_search's tool
    // result (id "1") but not yet read_file's (id "2") — proof the calls
    // ran in the order the fabricated trace issued them, not out of order
    // or both at once.
    let bodies = stub.bodies();
    let second_request: serde_json::Value = serde_json::from_str(&bodies[1]).expect("valid JSON body");
    let messages = second_request["messages"].as_array().expect("messages array");
    let tool_messages: Vec<&serde_json::Value> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_messages.len(), 1, "expected exactly one tool result before the second call");
    assert_eq!(tool_messages[0]["tool_call_id"], "1");
}

/// AC8: a fabricated malformed tool-call response continues the loop
/// without panicking; a fabricated unknown tool name likewise.
#[test]
fn ac8_malformed_and_unknown_tool_calls_continue_without_panicking() {
    let repo = temp_repo("ac8");
    let mut routes = HashMap::new();
    routes.insert(
        "/chat/completions",
        vec![
            // Malformed JSON arguments for a known tool.
            StubResponse::json(
                200,
                json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "1",
                                "type": "function",
                                "function": {"name": "read_file", "arguments": "not valid json"}
                            }]
                        }
                    }],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
                })
                .to_string(),
            ),
            // Unknown tool name.
            StubResponse::json(200, tool_call_response("2", "does_not_exist", json!({}))),
            StubResponse::json(200, summary_response("Recovered from both bad calls.")),
        ],
    );
    let stub = RoutedStubServer::start(routes);

    let index = Index::build(&repo).expect("index build should succeed");
    let client = Client::new(stub.base_url.clone(), "test-key".to_string());
    let mut log = open_log(&repo, "ac8");

    let cfg = LoopConfig {
        repo_root: repo.clone(),
        task: "hello".to_string(),
        model: "vendor/model".to_string(),
        max_iterations: 40,
        budget_tokens: 200_000,
        budget_usd: None,
        dry_run: false,
        keep_recent_turns: 0,
    };

    let (exit, _ledger) = run_loop(&cfg, &client, &index, &mut log);
    match exit {
        LoopExit::Completed { summary, iterations } => {
            assert_eq!(iterations, 3);
            assert!(summary.contains("Recovered"));
        }
        other => panic!("expected completion, got {other:?}"),
    }
}
