use std::fs;

mod common;

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

fn only_get_models_requests(requests: &[String]) {
    assert!(!requests.is_empty(), "expected at least one request to the stub");
    for r in requests {
        assert!(
            r.starts_with("GET /models"),
            "expected only GET /models requests (no completion call), saw: {r}"
        );
    }
}

/// AC5: a model lacking `tools` exits 4 with no completion call made.
#[test]
fn model_lacking_tools_exits_4_with_no_completion_call() {
    let stub = common::StubServer::start(
        r#"{"data":[{"id":"vendor/no-tools","context_length":32000,"supported_parameters":["temperature"]}]}"#,
    );
    let cwd = common::temp_dir("gate-no-tools");
    write_models_toml(&cwd, "vendor/no-tools");

    let output = common::codemason(&cwd)
        .args([
            "run",
            "--repo",
            cwd.to_str().unwrap(),
            "--task",
            "hello",
            "--model",
            "vendor/no-tools",
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", cwd.join("cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(4), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    only_get_models_requests(&stub.requests());
}

/// AC6: an id absent from the provider's live catalogue exits 4 (the id IS
/// in the allowlist — this is catalogue presence, not allowlist membership,
/// see PLAN.md "Ambiguity 2").
#[test]
fn id_absent_from_catalogue_exits_4() {
    let stub = common::StubServer::start(
        r#"{"data":[{"id":"vendor/other","context_length":32000,"supported_parameters":["tools"]}]}"#,
    );
    let cwd = common::temp_dir("gate-absent");
    write_models_toml(&cwd, "vendor/ghost");

    let output = common::codemason(&cwd)
        .args([
            "run",
            "--repo",
            cwd.to_str().unwrap(),
            "--task",
            "hello",
            "--model",
            "vendor/ghost",
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", cwd.join("cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(4), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    only_get_models_requests(&stub.requests());
}

/// AC7: an auto-router id exits 4 even though it's listed and otherwise
/// fully valid.
#[test]
fn auto_router_id_exits_4() {
    let stub = common::StubServer::start(
        r#"{"data":[{"id":"openrouter/auto","context_length":200000,"supported_parameters":["tools"]}]}"#,
    );
    let cwd = common::temp_dir("gate-router");
    write_models_toml(&cwd, "openrouter/auto");

    let output = common::codemason(&cwd)
        .args([
            "run",
            "--repo",
            cwd.to_str().unwrap(),
            "--task",
            "hello",
            "--model",
            "openrouter/auto",
            "--base-url",
            &stub.base_url,
            "--api-key",
            "test-key",
        ])
        .env("CODEMASON_CACHE_DIR", cwd.join("cache"))
        .output()
        .expect("run codemason run");

    assert_eq!(output.status.code(), Some(4), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    only_get_models_requests(&stub.requests());
}

/// AC8: an unlisted id exits 4 without `--allow-unlisted-model` and proceeds
/// (exit 0) with it, given the id is otherwise valid in the live catalogue.
///
/// Exit 0 now means the whole WP3 loop ran to completion, not just that
/// gating passed (that was WP2's contract, before `run` did anything past
/// gating) — so this needs a repo with a real source file plus a
/// `/chat/completions` route that terminates immediately with no tool
/// calls, not just a `/models` catalogue.
#[test]
fn unlisted_id_exits_4_without_flag_and_proceeds_with_it() {
    let cwd = common::temp_dir("gate-unlisted");
    std::fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "/models",
        vec![common::StubResponse::json(
            200,
            r#"{"data":[{"id":"vendor/real-model","context_length":32000,"supported_parameters":["tools"]}]}"#,
        )],
    );
    routes.insert(
        "/chat/completions",
        vec![common::StubResponse::json(
            200,
            r#"{"choices":[{"message":{"role":"assistant","content":"done"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        )],
    );
    let stub = common::RoutedStubServer::start(routes);
    // The allowlist has a different id — "vendor/real-model" is unlisted.
    write_models_toml(&cwd, "vendor/other-model");

    let base_args = [
        "run",
        "--repo",
        cwd.to_str().unwrap(),
        "--task",
        "hello",
        "--model",
        "vendor/real-model",
        "--base-url",
        &stub.base_url,
        "--api-key",
        "test-key",
    ];

    let without_flag = common::codemason(&cwd)
        .args(base_args)
        .env("CODEMASON_CACHE_DIR", cwd.join("cache-a"))
        .output()
        .expect("run without flag");
    assert_eq!(
        without_flag.status.code(),
        Some(4),
        "stderr: {}",
        String::from_utf8_lossy(&without_flag.stderr)
    );

    let with_flag = common::codemason(&cwd)
        .args(base_args)
        .arg("--allow-unlisted-model")
        .env("CODEMASON_CACHE_DIR", cwd.join("cache-b"))
        .output()
        .expect("run with flag");
    assert_eq!(
        with_flag.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&with_flag.stderr)
    );
}

/// AC9: a second invocation within the TTL makes no network call.
///
/// Exit 0 now means the whole WP3 loop ran to completion (see the comment
/// on `unlisted_id_exits_4_without_flag_and_proceeds_with_it`), so both
/// runs need a repo with a real source file and a `/chat/completions` route
/// that terminates immediately — the assertion under test is still only
/// about the `/models` fetch count.
#[test]
fn second_invocation_within_ttl_makes_no_network_call() {
    let cwd = common::temp_dir("gate-cache-ttl");
    std::fs::write(cwd.join("a.rs"), "fn main() {}\n").unwrap();
    let mut routes = std::collections::HashMap::new();
    routes.insert(
        "/models",
        vec![common::StubResponse::json(
            200,
            r#"{"data":[{"id":"vendor/cached-model","context_length":32000,"supported_parameters":["tools"]}]}"#,
        )],
    );
    routes.insert(
        "/chat/completions",
        vec![
            common::StubResponse::json(
                200,
                r#"{"choices":[{"message":{"role":"assistant","content":"done"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            ),
            common::StubResponse::json(
                200,
                r#"{"choices":[{"message":{"role":"assistant","content":"done"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            ),
        ],
    );
    let stub = common::RoutedStubServer::start(routes);
    write_models_toml(&cwd, "vendor/cached-model");
    let cache_dir = cwd.join("cache");

    let args = [
        "run",
        "--repo",
        cwd.to_str().unwrap(),
        "--task",
        "hello",
        "--model",
        "vendor/cached-model",
        "--base-url",
        &stub.base_url,
        "--api-key",
        "test-key",
    ];

    let first = common::codemason(&cwd)
        .args(args)
        .env("CODEMASON_CACHE_DIR", &cache_dir)
        .output()
        .expect("first run");
    assert_eq!(first.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&first.stderr));

    let second = common::codemason(&cwd)
        .args(args)
        .env("CODEMASON_CACHE_DIR", &cache_dir)
        .output()
        .expect("second run");
    assert_eq!(second.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&second.stderr));

    let catalogue_requests: Vec<String> = stub
        .requests()
        .into_iter()
        .filter(|r| r.contains("/models"))
        .collect();
    assert_eq!(
        catalogue_requests.len(),
        1,
        "expected exactly one catalogue fetch across both runs, got {catalogue_requests:?}"
    );
}
