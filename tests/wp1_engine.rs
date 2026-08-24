//! Consolidates two WP1 acceptance criteria that predate this test tree and
//! had no automated check until WP5: AC2 (dependency-tree shape) and AC7
//! (the vendored engine is byte-identical to what was supplied). See
//! PLAN.md's WP5 Feasibility section for how each was investigated.

use std::path::{Path, PathBuf};
use std::process::Command;

/// AC2: `cargo tree` must show no `openssl`, `git2`, async runtime, or
/// out-of-repository path/git dependency.
///
/// The AC's literal text also asks for no embedding or array crate at all,
/// but `model2vec-rs`/`ndarray` are unconditional dependencies in this
/// tree's `Cargo.toml` — a documented WP1 trade-off, not an oversight: the
/// vendored `src/engine/mod.rs` declares `pub mod encoder;` with no `cfg`
/// gate, and `src/engine/encoder.rs` unconditionally imports both crates, so
/// making them `optional = true` would break the *default* `cargo build`
/// (AC1) instead of fixing AC2 — and editing the vendored source to add a
/// gate is forbidden by CLAUDE.md's do-not-refactor rule. Resolved with the
/// developer during WP5: this test asserts the contract WP1 actually
/// shipped (no forbidden crates, no external dependency; embedding
/// *functionality*, not crate presence, is what's gated — see
/// `src/index.rs`'s `ac6_similarity_call_names_the_missing_feature...`
/// test) rather than the AC's literal wording.
#[test]
fn ac2_default_tree_excludes_forbidden_crates_and_external_deps() {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "-e", "normal"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);

    for forbidden in ["openssl", "git2", "tokio", "async-std"] {
        assert!(
            !tree.to_lowercase().contains(forbidden),
            "cargo tree must not contain {forbidden:?}:\n{tree}"
        );
    }

    // A path or git dependency outside this repository shows up as
    // `(path/to/...)` or a `git+...` source annotation on a tree line; the
    // only path annotation expected is this crate's own root, printed on
    // the first line as `(C:\...\codemason)`.
    for line in tree.lines().skip(1) {
        assert!(
            !line.contains("(git+") && !line.to_lowercase().contains("(path:"),
            "unexpected external path/git dependency: {line}"
        );
    }
}

fn collect_relative_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(path.strip_prefix(root).expect("strip_prefix").to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// AC7: a recursive diff of `src/engine/` against the supplied source tree
/// is empty. The supplied tree lives at
/// `old_source/navex-harness-main/harness/crates/lib/context/src/engine` —
/// identified by grepping `old_source` for the engine's own module names
/// (`bm25.rs`, `chunking.rs`, `outline.rs`) — and is gitignored (local
/// reference only), so this test skips when it isn't present on the
/// checkout, matching `tests/index_stats.rs`'s existing convention for the
/// same fixture tree.
#[test]
fn ac7_engine_tree_matches_the_supplied_source_byte_for_byte() {
    let vendored = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
    let supplied = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("old_source/navex-harness-main/harness/crates/lib/context/src/engine");

    if !supplied.exists() {
        eprintln!("skipping: {} not present", supplied.display());
        return;
    }

    let vendored_files = collect_relative_files(&vendored);
    let supplied_files = collect_relative_files(&supplied);
    assert_eq!(
        vendored_files, supplied_files,
        "file sets differ between src/engine/ and the supplied source tree"
    );

    for rel in &vendored_files {
        let a = std::fs::read(vendored.join(rel)).expect("read vendored file");
        let b = std::fs::read(supplied.join(rel)).expect("read supplied file");
        assert_eq!(a, b, "content differs for {}", rel.display());
    }
}
