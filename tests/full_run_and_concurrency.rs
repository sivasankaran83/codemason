//! T5.3's two named additions beyond consolidating WP1–4's own ACs: a
//! dedicated full-successful-run smoke test, and the concurrency case —
//! "the concurrency case validates the premise of the whole project and
//! must not be deferred" (SPEC.md, T5.3). Also WP5's own AC4.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use serde_json::json;

mod common;
use common::{assert_single_json_report, codemason, temp_dir, RoutedStubServer, StubResponse};

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

/// One clone's worth of fixture setup: a repo with one seed file and a
/// models.toml, ready for `codemason run` to be pointed at it.
fn setup_clone(label: &str, model_id: &str, seed_content: &str) -> std::path::PathBuf {
    let cwd = temp_dir(label);
    fs::write(cwd.join("a.rs"), seed_content).unwrap();
    write_models_toml(&cwd, model_id);
    common::init_git_repo(&cwd);
    cwd
}

fn routes_for(model_id: &str, write_content: &str) -> HashMap<&'static str, Vec<StubResponse>> {
    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model_id))]);
    routes.insert(
        "/chat/completions",
        vec![
            StubResponse::json(
                200,
                tool_call_response("1", "write_file", json!({"path": "a.rs", "content": write_content})),
            ),
            StubResponse::json(200, summary_response("done")),
        ],
    );
    routes
}

/// T5.3: "a full successful run: exit 0, branch, commit, valid stdout JSON."
/// Same underlying path as WP4's `ac6_run_commits_when_something_changed`,
/// named and asserted here for direct traceability to this specific T5.3
/// line item.
#[test]
fn wp5_full_successful_run_exit0_branch_commit_valid_json() {
    let model_id = "vendor/wp5-full-run-model";
    let cwd = setup_clone("wp5-full-run", model_id, "fn main() {}\n");
    let branch_before = current_branch(&cwd);
    let head_before = current_head(&cwd);

    let stub = RoutedStubServer::start(routes_for(model_id, "fn main() { /* wp5 */ }\n"));

    let output = codemason(&cwd)
        .args([
            "run",
            "--repo",
            cwd.to_str().unwrap(),
            "--task",
            "wp5 full run",
            "--model",
            model_id,
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("wp5-full-run-cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let report = assert_single_json_report(&output.stdout, 0);
    let branch = report["branch"].as_str().expect("report.branch present");
    let commit = report["commit"].as_str().expect("report.commit present");
    assert_ne!(branch, branch_before, "run must create/checkout a new branch");
    assert_ne!(current_head(&cwd), head_before, "run must produce a new commit");
    assert_eq!(current_head(&cwd), commit, "report.commit must match the actual HEAD");
}

/// WP5/AC4, and T5.3's named concurrency case: two independent
/// `codemason run` processes, each against its own clone and its own stub
/// server, spawned before either is waited on, must both succeed with
/// independent branches, commits and logs — none of one run's state
/// observable in the other's.
#[test]
fn wp5_ac4_two_concurrent_runs_against_two_clones_succeed_independently() {
    let model_a = "vendor/wp5-concurrent-a";
    let model_b = "vendor/wp5-concurrent-b";
    let cwd_a = setup_clone("wp5-concurrent-a", model_a, "fn main() {}\n");
    let cwd_b = setup_clone("wp5-concurrent-b", model_b, "fn main() {}\n");

    let stub_a = RoutedStubServer::start(routes_for(model_a, "fn main() { /* a */ }\n"));
    let stub_b = RoutedStubServer::start(routes_for(model_b, "fn main() { /* b */ }\n"));

    let child_a = codemason(&cwd_a)
        .args([
            "run",
            "--repo",
            cwd_a.to_str().unwrap(),
            "--task",
            "concurrent a",
            "--model",
            model_a,
            "--base-url",
            &stub_a.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("wp5-concurrent-a-cache"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn run a");

    let child_b = codemason(&cwd_b)
        .args([
            "run",
            "--repo",
            cwd_b.to_str().unwrap(),
            "--task",
            "concurrent b",
            "--model",
            model_b,
            "--base-url",
            &stub_b.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("wp5-concurrent-b-cache"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn run b");

    // Neither child is waited on until both are running — the point of the
    // test is that they overlap, not that they happen to run back to back.
    let output_a = child_a.wait_with_output().expect("wait run a");
    let output_b = child_b.wait_with_output().expect("wait run b");

    assert_eq!(output_a.status.code(), Some(0), "run a stderr: {}", String::from_utf8_lossy(&output_a.stderr));
    assert_eq!(output_b.status.code(), Some(0), "run b stderr: {}", String::from_utf8_lossy(&output_b.stderr));

    let report_a = assert_single_json_report(&output_a.stdout, 0);
    let report_b = assert_single_json_report(&output_b.stdout, 0);

    let branch_a = report_a["branch"].as_str().expect("run a report.branch");
    let branch_b = report_b["branch"].as_str().expect("run b report.branch");
    let commit_a = report_a["commit"].as_str().expect("run a report.commit");
    let commit_b = report_b["commit"].as_str().expect("run b report.commit");
    let run_id_a = report_a["run_id"].as_str().expect("run a report.run_id");
    let run_id_b = report_b["run_id"].as_str().expect("run b report.run_id");

    assert_ne!(branch_a, branch_b, "each run must have its own branch");
    assert_ne!(commit_a, commit_b, "each run must have its own commit");
    assert_ne!(run_id_a, run_id_b, "each run must have its own run_id");

    assert_eq!(current_head(&cwd_a), commit_a);
    assert_eq!(current_head(&cwd_b), commit_b);
    assert_eq!(
        fs::read_to_string(cwd_a.join("a.rs")).unwrap(),
        "fn main() { /* a */ }\n",
        "run a's write must not leak into clone b"
    );
    assert_eq!(
        fs::read_to_string(cwd_b.join("a.rs")).unwrap(),
        "fn main() { /* b */ }\n",
        "run b's write must not leak into clone a"
    );
}
