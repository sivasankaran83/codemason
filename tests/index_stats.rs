mod common;

/// AC2: `codemason index --repo . --stats` works end to end.
///
/// Uses the same on-disk-only `old_source` C# fixture WP1's own tests used
/// (see PLAN.md risk flag 4 there) — real repo, not committed, so this test
/// only exercises on a checkout that has it.
#[test]
fn index_stats_end_to_end_against_a_real_repo() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("old_source/navex-harness-main/metric-measurement-service");
    if !fixture.exists() {
        eprintln!("skipping: {} not present", fixture.display());
        return;
    }

    let cwd = common::temp_dir("index-stats");
    let output = common::codemason(&cwd)
        .args(["index", "--repo", fixture.to_str().unwrap(), "--stats"])
        .output()
        .expect("run index --stats");

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("indexed_files"), "missing indexed_files in:\n{stdout}");
    assert!(stdout.contains("total_chunks"), "missing total_chunks in:\n{stdout}");
    assert!(!stdout.contains("indexed_files: 0"), "expected a plausible (non-zero) file count:\n{stdout}");
    assert!(!stdout.contains("total_chunks: 0"), "expected a plausible (non-zero) chunk count:\n{stdout}");
}
