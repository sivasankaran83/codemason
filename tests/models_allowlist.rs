use std::fs;

mod common;

const ORDERED_TOML: &str = r#"
[[model]]
id = "vendor/model-a"
role = "primary"

[[model]]
id = "vendor/model-b"
role = "fallback"

[gating]
min_context_length = 8000
require_tool_support = true
allow_unlisted = false
"#;

/// AC3 (ordering): `codemason models` prints the allowlist in declaration
/// order.
#[test]
fn models_prints_allowlist_in_order() {
    let cwd = common::temp_dir("models-ordered");
    fs::write(cwd.join("models.toml"), ORDERED_TOML).unwrap();

    let output = common::codemason(&cwd).arg("models").output().expect("run models");
    assert!(output.status.success(), "expected exit 0: {output:?}");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pos_a = stdout.find("vendor/model-a").expect("model-a listed");
    let pos_b = stdout.find("vendor/model-b").expect("model-b listed");
    assert!(pos_a < pos_b, "expected model-a before model-b:\n{stdout}");
}

/// AC3 (malformed): malformed TOML exits 1 naming the file and the parse
/// error.
#[test]
fn models_malformed_toml_exits_1_naming_file_and_error() {
    let cwd = common::temp_dir("models-malformed");
    let config_path = cwd.join("models.toml");
    fs::write(&config_path, "this is not [ valid toml").unwrap();

    let output = common::codemason(&cwd).arg("models").output().expect("run models");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(config_path.file_name().unwrap().to_str().unwrap()),
        "expected the file name in the error:\n{stderr}"
    );
}

/// AC3 (missing): a `models.toml` missing from every searched location
/// exits 1 listing the paths searched.
///
/// This relies on the machine's platform config dir
/// (`dirs::config_dir()/codemason/models.toml`) genuinely not existing —
/// there's no portable way to override `dirs::config_dir()` for a child
/// process on Windows (see gating.rs's `CODEMASON_CACHE_DIR` comment for the
/// same limitation applied to the cache dir). True on a clean checkout;
/// would need a real override mechanism to be airtight in CI.
#[test]
fn models_missing_everywhere_exits_1_listing_searched_paths() {
    let platform_config = dirs::config_dir().map(|d| d.join("codemason").join("models.toml"));
    if platform_config.as_deref().is_some_and(|p| p.exists()) {
        eprintln!("skipping: a real platform models.toml exists on this machine");
        return;
    }

    let cwd = common::temp_dir("models-missing");
    let output = common::codemason(&cwd).arg("models").output().expect("run models");
    assert_eq!(output.status.code(), Some(1), "expected exit 1: {output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("models.toml"), "expected searched paths in error:\n{stderr}");
}
