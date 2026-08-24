mod common;

/// AC1: `codemason --help` lists three subcommands; every documented flag
/// appears.
#[test]
fn help_lists_subcommands_and_documented_flags() {
    let cwd = common::temp_dir("help");
    let output = common::codemason(&cwd)
        .arg("--help")
        .output()
        .expect("run --help");
    let top = String::from_utf8_lossy(&output.stdout);

    for subcommand in ["run", "models", "index"] {
        assert!(top.contains(subcommand), "top-level help missing subcommand {subcommand:?}:\n{top}");
    }

    let run_help = common::codemason(&cwd)
        .args(["run", "--help"])
        .output()
        .expect("run run --help");
    let run_help = String::from_utf8_lossy(&run_help.stdout);
    for flag in [
        "--repo",
        "--task",
        "--model",
        "--models-config",
        "--base-url",
        "--api-key",
        "--budget-tokens",
        "--budget-usd",
        "--max-iterations",
        "--branch",
        "--log",
        "--dry-run",
        "--allow-unlisted-model",
        "--verbose",
    ] {
        assert!(run_help.contains(flag), "`run --help` missing flag {flag:?}:\n{run_help}");
    }

    let models_help = common::codemason(&cwd)
        .args(["models", "--help"])
        .output()
        .expect("run models --help");
    let models_help = String::from_utf8_lossy(&models_help.stdout);
    assert!(models_help.contains("--check"), "`models --help` missing --check:\n{models_help}");

    let index_help = common::codemason(&cwd)
        .args(["index", "--help"])
        .output()
        .expect("run index --help");
    let index_help = String::from_utf8_lossy(&index_help.stdout);
    for flag in ["--repo", "--stats"] {
        assert!(index_help.contains(flag), "`index --help` missing flag {flag:?}:\n{index_help}");
    }
}
