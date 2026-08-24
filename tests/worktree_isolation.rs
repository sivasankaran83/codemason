//! Worktree isolation: two concurrent runs against two *sections of one
//! clone* — the monorepo case.
//!
//! Two runs against two separate clones were always safe and are covered by
//! `full_run_and_concurrency.rs`. Sharing a clone is what is not safe
//! without `--worktree`, and it fails in a way that matters: the surviving
//! run commits the other run's in-flight edits and reports a branch that
//! does not hold its own commit. The final assertion here — reported branch
//! resolves to reported commit — is the one that would have caught that.

use std::collections::HashMap;
use std::fs;
use std::process::Command;

use serde_json::json;

mod common;
use common::{assert_single_json_report, codemason, temp_dir, RoutedStubServer, StubResponse};

fn write_models_toml(dir: &std::path::Path, model_id: &str) {
    fs::write(
        dir.join("models.toml"),
        format!(
            r#"
[[model]]
id = "{model_id}"
role = "primary"

[gating]
min_context_length = 8000
require_tool_support = true
allow_unlisted = false
"#
        ),
    )
    .unwrap();
}

fn catalogue_body(model_id: &str) -> String {
    json!({"data": [{"id": model_id, "context_length": 32000, "supported_parameters": ["tools"]}]})
        .to_string()
}

fn write_then_summarize(path: &str, content: &str) -> Vec<StubResponse> {
    vec![
        StubResponse::json(
            200,
            json!({
                "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "1", "type": "function",
                    "function": {"name": "write_file",
                                 "arguments": json!({"path": path, "content": content}).to_string()}
                }]}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
            })
            .to_string(),
        ),
        StubResponse::json(
            200,
            json!({
                "choices": [{"message": {"role": "assistant", "content": format!("updated {path}")}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
            })
            .to_string(),
        ),
    ]
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A monorepo with two independent service sections.
fn monorepo(label: &str) -> std::path::PathBuf {
    let root = temp_dir(label);
    for svc in ["orders", "billing"] {
        let src = root.join("services").join(svc).join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("Service.cs"), format!("namespace {svc};\npublic class S {{ public int V() => 0; }}\n")).unwrap();
    }
    common::init_git_repo(&root);
    root
}

#[test]
fn concurrent_sections_of_one_clone_stay_isolated_under_worktree() {
    let root = monorepo("wt-mono");
    let model_a = "vendor/wt-orders";
    let model_b = "vendor/wt-billing";

    let cwd_a = temp_dir("wt-cwd-a");
    let cwd_b = temp_dir("wt-cwd-b");
    write_models_toml(&cwd_a, model_a);
    write_models_toml(&cwd_b, model_b);

    let mut routes_a = HashMap::new();
    routes_a.insert("/models", vec![StubResponse::json(200, catalogue_body(model_a))]);
    routes_a.insert(
        "/chat/completions",
        write_then_summarize("src/Service.cs", "namespace orders;\npublic class S { public int V() => 7; }\n"),
    );
    let stub_a = RoutedStubServer::start(routes_a);

    let mut routes_b = HashMap::new();
    routes_b.insert("/models", vec![StubResponse::json(200, catalogue_body(model_b))]);
    routes_b.insert(
        "/chat/completions",
        write_then_summarize("src/Service.cs", "namespace billing;\npublic class S { public int V() => 8; }\n"),
    );
    let stub_b = RoutedStubServer::start(routes_b);

    let spawn = |cwd: &std::path::Path, section: &str, model: &str, base: &str, cache: &str| {
        codemason(cwd)
            .args([
                "run",
                "--repo",
                root.join("services").join(section).to_str().unwrap(),
                "--worktree",
                "--task",
                section,
                "--model",
                model,
                "--base-url",
                base,
                "--api-key",
                "test-key",
            ])
            .env("CODEMASON_CACHE_DIR", temp_dir(cache))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn run")
    };

    // Both in flight before either is awaited — the overlap is the test.
    let a = spawn(&cwd_a, "orders", model_a, &stub_a.base_url, "wt-cache-a");
    let b = spawn(&cwd_b, "billing", model_b, &stub_b.base_url, "wt-cache-b");
    let out_a = a.wait_with_output().expect("wait a");
    let out_b = b.wait_with_output().expect("wait b");

    assert_eq!(out_a.status.code(), Some(0), "orders stderr: {}", String::from_utf8_lossy(&out_a.stderr));
    assert_eq!(out_b.status.code(), Some(0), "billing stderr: {}", String::from_utf8_lossy(&out_b.stderr));

    let rep_a = assert_single_json_report(&out_a.stdout, 0);
    let rep_b = assert_single_json_report(&out_b.stdout, 0);

    // Each run committed ONLY its own section — no cross-contamination.
    assert_eq!(rep_a["files_changed"], json!(["services/orders/src/Service.cs"]));
    assert_eq!(rep_b["files_changed"], json!(["services/billing/src/Service.cs"]));

    // The reported branch actually holds the reported commit. Without
    // isolation this is where the contract breaks: the commit lands on the
    // other run's branch while the report names this one.
    for rep in [&rep_a, &rep_b] {
        let branch = rep["branch"].as_str().expect("branch reported");
        let commit = rep["commit"].as_str().expect("commit reported");
        assert_eq!(
            git(&root, &["rev-parse", branch]),
            commit,
            "reported branch {branch} must resolve to reported commit {commit}"
        );
    }

    // Worktrees are torn down; only the original tree remains.
    let worktrees = git(&root, &["worktree", "list"]);
    assert_eq!(
        worktrees.lines().count(),
        1,
        "worktrees must be removed after the run; got:\n{worktrees}"
    );

    // Both branches merge cleanly into the base and both changes survive.
    let base = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(!base.is_empty());
    for rep in [&rep_a, &rep_b] {
        let branch = rep["branch"].as_str().unwrap();
        let out = Command::new("git")
            .args(["merge", "--no-edit", branch])
            .current_dir(&root)
            .output()
            .expect("merge");
        assert!(out.status.success(), "merging {branch} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(fs::read_to_string(root.join("services/orders/src/Service.cs")).unwrap().contains("=> 7"));
    assert!(fs::read_to_string(root.join("services/billing/src/Service.cs")).unwrap().contains("=> 8"));
}

/// The event log is written under the *original* repository path, not the
/// worktree — otherwise removing the worktree would delete the run's own
/// diagnostic record along with it.
#[test]
fn event_log_survives_worktree_teardown() {
    let root = monorepo("wt-log");
    let model = "vendor/wt-log";
    let cwd = temp_dir("wt-log-cwd");
    write_models_toml(&cwd, model);

    let mut routes = HashMap::new();
    routes.insert("/models", vec![StubResponse::json(200, catalogue_body(model))]);
    routes.insert(
        "/chat/completions",
        write_then_summarize("src/Service.cs", "namespace orders;\npublic class S { public int V() => 1; }\n"),
    );
    let stub = RoutedStubServer::start(routes);

    let out = codemason(&cwd)
        .args([
            "run",
            "--repo",
            root.join("services/orders").to_str().unwrap(),
            "--worktree",
            "--task",
            "log survival",
            "--model",
            model,
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", temp_dir("wt-log-cache"))
        .output()
        .expect("run");

    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let rep = assert_single_json_report(&out.stdout, 0);

    let log_path = rep["log_path"].as_str().expect("log_path reported");
    assert!(
        std::path::Path::new(log_path).exists(),
        "event log must outlive the worktree it was produced in: {log_path}"
    );

    // And the run's own account of what it did reaches stdout, not just stderr.
    let summary = rep["summary"].as_str().expect("summary reported");
    assert!(!summary.trim().is_empty(), "summary must not be empty on a completed run");
}
