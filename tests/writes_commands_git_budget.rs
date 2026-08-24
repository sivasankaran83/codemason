use std::collections::HashMap;
use std::fs;
use std::process::Command;

use serde_json::{json, Value};

mod common;
use common::{assert_single_json_report, codemason, temp_dir, RoutedStubServer, StubResponse, StubServer};

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

fn tool_call_response(id: &str, tool: &str, args: Value) -> String {
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

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git").args(args).current_dir(dir).output().expect("run git");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn current_branch(dir: &std::path::Path) -> String {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
}

fn current_head(dir: &std::path::Path) -> String {
    git(dir, &["rev-parse", "HEAD"])
}

fn commit_count(dir: &std::path::Path) -> usize {
    git(dir, &["log", "--oneline"]).lines().filter(|l| !l.is_empty()).count()
}

/// AC3: `--dry-run` leaves the filesystem and git state untouched while the
/// loop still completes.
#[test]
fn ac3_dry_run_leaves_filesystem_and_git_untouched() {
    let cwd = temp_dir("dry-run");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/dry-run-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let branch_before = current_branch(&cwd);
    let head_before = current_head(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(
                200,
                tool_call_response("1", "write_file", json!({"path": "a.rs", "content": "fn main() { /* changed */ }\n"})),
            ),
            StubResponse::json(200, summary_response("done")),
        ],
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
            "--dry-run",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("dry-run-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let content = fs::read_to_string(cwd.join("a.rs")).unwrap();
    assert_eq!(content, "fn main() {}\n", "dry-run must not modify the file");
    assert_eq!(current_branch(&cwd), branch_before, "dry-run must not create/checkout a branch");
    assert_eq!(current_head(&cwd), head_before, "dry-run must not commit");

    let report = assert_single_json_report(&output.stdout, 0);
    assert_eq!(report["branch"], Value::Null);
    assert_eq!(report["commit"], Value::Null);
}

/// AC6 (part 1): a successful run that writes a file leaves a branch with
/// exactly one new commit containing the intended change.
#[test]
fn ac6_run_commits_when_something_changed() {
    let cwd = temp_dir("commit-on-write");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/commit-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);
    let commits_before = commit_count(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(
                200,
                tool_call_response("1", "write_file", json!({"path": "a.rs", "content": "fn main() { /* changed */ }\n"})),
            ),
            StubResponse::json(200, summary_response("done")),
        ],
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
        .env("CODEMASON_CACHE_DIR", temp_dir("commit-on-write-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(commit_count(&cwd), commits_before + 1, "expected exactly one new commit");

    let content = fs::read_to_string(cwd.join("a.rs")).unwrap();
    assert_eq!(content, "fn main() { /* changed */ }\n");

    let report = assert_single_json_report(&output.stdout, 0);
    assert!(report["commit"].as_str().is_some(), "report: {report}");
    assert_eq!(report["files_changed"], json!(["a.rs"]));
    assert!(report["branch"].as_str().is_some());
}

/// AC6 (part 2): a run that changes nothing produces no commit.
#[test]
fn ac6_no_op_run_produces_no_commit() {
    let cwd = temp_dir("no-op-run");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/no-op-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);
    let commits_before = commit_count(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![StubResponse::json(200, summary_response("nothing to do"))],
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
        .env("CODEMASON_CACHE_DIR", temp_dir("no-op-run-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(commit_count(&cwd), commits_before, "a no-op run must not add a commit");

    let report = assert_single_json_report(&output.stdout, 0);
    assert_eq!(report["commit"], Value::Null);
    assert_eq!(report["files_changed"], json!([]));
}

/// AC7: a dirty worktree without `--dry-run` exits 1 before any API call.
#[test]
fn ac7_dirty_worktree_exits_1_before_any_api_call() {
    let cwd = temp_dir("git-dirty");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/dirty-check";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);
    // Uncommitted change after the initial commit — the tree is now dirty.
    fs::write(cwd.join("a.rs"), "fn main() { /* dirty */ }\n").unwrap();

    let stub = StubServer::start(r#"{"data":[]}"#);

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
        .env("CODEMASON_CACHE_DIR", temp_dir("git-dirty-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(1), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(
        stub.requests().is_empty(),
        "expected zero requests before the dirty-worktree check, saw {:?}",
        stub.requests()
    );

    assert_single_json_report(&output.stdout, 1);
}

/// AC8 (part 1): `--budget-tokens 0` refuses to start — exits 2 with zero
/// API calls. Per the resolved ambiguity (see PLAN.md), the "zero calls"
/// case is exercised at `0`, not the AC's literal `1` — a budget of `1`
/// necessarily allows exactly one call before the breach is visible.
#[test]
fn ac8_budget_tokens_zero_exits_2_with_zero_calls() {
    let cwd = temp_dir("budget-zero");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/budget-zero-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
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
            "--budget-tokens",
            "0",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("budget-zero-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let completions = stub.requests().into_iter().filter(|r| r.contains("/chat/completions")).count();
    assert_eq!(completions, 0, "expected zero completion calls");

    assert_single_json_report(&output.stdout, 2);
}

/// AC8 (part 2): a small nonzero budget allows exactly one call — the
/// breach, once the first response's usage is known, is caught before the
/// second call — and partial work from that one call is still committed.
#[test]
fn ac8_small_budget_allows_one_call_then_breaches_with_partial_commit() {
    let cwd = temp_dir("budget-small");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/budget-small-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let big_usage_write = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "1",
                    "type": "function",
                    "function": {"name": "write_file", "arguments": json!({"path": "a.rs", "content": "fn main() { /* changed */ }\n"}).to_string()}
                }]
            }
        }],
        "usage": {"prompt_tokens": 500, "completion_tokens": 500, "total_tokens": 1000}
    })
    .to_string();

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert("/chat/completions", vec![StubResponse::json(200, big_usage_write)]);
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
            "--budget-tokens",
            "10",
            "--log",
            log_path.to_str().unwrap(),
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("budget-small-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(2), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let completions = stub.requests().into_iter().filter(|r| r.contains("/chat/completions")).count();
    assert_eq!(completions, 1, "expected exactly one completion call before the breach");

    let content = fs::read_to_string(cwd.join("a.rs")).unwrap();
    assert_eq!(content, "fn main() { /* changed */ }\n", "the one completed write should still be committed");

    let report = assert_single_json_report(&output.stdout, 2);
    assert!(report["commit"].as_str().is_some(), "partial work should be committed; report: {report}");
    assert_eq!(report["totals"]["total_tokens"], json!(1000));

    // AC8: "totals matching the sum of llm_call events" — cross-check the
    // report against the independently-written event log, not just against
    // itself.
    let log_text = fs::read_to_string(&log_path).unwrap();
    let logged_total: u64 = log_text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["type"] == "llm_call")
        .map(|event| event["total_tokens"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(
        report["totals"]["total_tokens"].as_u64().unwrap(),
        logged_total,
        "report totals must reconcile exactly with the sum of llm_call events"
    );
}

/// AC8 (part 3): `--max-iterations 2` on a task with more available turns
/// stops after exactly two calls, exits 3, and commits the partial work.
#[test]
fn ac8_max_iterations_stops_after_two_calls_with_partial_commit() {
    let cwd = temp_dir("max-iterations");
    fs::write(cwd.join("seed.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/max-iter-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(200, tool_call_response("1", "write_file", json!({"path": "one.txt", "content": "first\n"}))),
            StubResponse::json(200, tool_call_response("2", "write_file", json!({"path": "two.txt", "content": "second\n"}))),
            // Never reached — max_iterations=2 stops before a third call.
            StubResponse::json(200, tool_call_response("3", "write_file", json!({"path": "three.txt", "content": "third\n"}))),
        ],
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
            "--max-iterations",
            "2",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("max-iterations-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(3), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let completions = stub.requests().into_iter().filter(|r| r.contains("/chat/completions")).count();
    assert_eq!(completions, 2, "expected exactly two completion calls");

    assert!(cwd.join("one.txt").exists());
    assert!(cwd.join("two.txt").exists());
    assert!(!cwd.join("three.txt").exists(), "a third call must never have happened");

    let report = assert_single_json_report(&output.stdout, 3);
    assert!(report["commit"].as_str().is_some());
    let mut files: Vec<String> = report["files_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    files.sort();
    assert_eq!(files, vec!["one.txt".to_string(), "two.txt".to_string()]);
}

/// AC9: `--verbose` adds nothing to stdout — only the one JSON report line.
#[test]
fn ac9_verbose_adds_nothing_to_stdout() {
    let cwd = temp_dir("verbose");
    fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let model_id = "vendor/verbose-model";
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert("/chat/completions", vec![StubResponse::json(200, summary_response("done"))]);
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
            "--verbose",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("verbose-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_single_json_report(&output.stdout, 0);
    // --verbose's extra diagnostics land on stderr, not stdout.
    assert!(!String::from_utf8_lossy(&output.stderr).is_empty());
}
