# PLAN.md — WP1: Harvest the engine

## Scope (restated)

Stand up the codemason cargo project and vendor the supplied AST search
engine into it unmodified. Three tasks:

- **T1.1** Project skeleton: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`,
  `README.md` (README already exists at repo root — leave it, it already
  documents WP1's shape).
- **T1.2** Lift the engine verbatim into `src/engine/`, write a thin
  `src/lib.rs` re-exporting its declared public surface, write
  `src/error.rs` defining `codemason_core::Error` with no reference to any
  external crate's error/exit-code type, carry `ORIGIN.md` and the licence
  file.
- **T1.3** `src/index.rs`: a thin wrapper over the engine's index
  constructor called with `encoder: None`, exposing build/search/chunks/
  graph/stats, timing the build into the stats struct, and returning a
  named-feature error (not a panic) for the embedding-only similarity call
  when the `embeddings` feature is off.

Everything downstream (CLI, gating, tools, loop) is later work packages.
WP1 produces a crate that builds and searches; it has no `main` behaviour
beyond what's needed to prove the engine works.

## Feasibility findings

The engine source is not in this repository. Per your message, it lives at
`old_source/navex-harness-main/harness/crates/lib/context/src/engine/`
inside a sibling project (`navex-harness`, a different Rust CLI by the same
author). That folder is a **reference only** — nothing under `old_source/`
is committed, staged, or depended on by path; it is not part of this crate's
build. I copy its `engine/` subtree byte-for-byte into `src/engine/` and
never touch `old_source/` again after that copy.

Every claim in SPEC.md's Current State section checks out against that
source:

| Claim | Verified |
|---|---|
| Zero `use crate::` refs outside `engine/` | Confirmed — every `use crate::` inside `engine/` resolves to `crate::engine::...`; none reach the parent crate. |
| Only 3 external `ExitCode` uses, all outside `engine/` | Confirmed — `src/error.rs` (parent crate, not vendored) has exactly three production call sites (`MissingPrerequisite`, `Failure` x2); `engine/` has none. |
| Encoder already `Option<StaticEncoder>` in index modules | Confirmed — `index/mod.rs` and `index/create.rs` both thread it as `Option<&StaticEncoder>` / `Option<StaticEncoder>`. |
| CLI entry point already stripped | Confirmed — no `fn main`/`clap::` in `engine/`; the two grep hits were a test fixture string and a heuristic string-prefix check, not real entry points. |
| No persistence: `Serialize` but not `Deserialize` on chunk types | Confirmed — `types.rs` derives `serde::Serialize` only on `Chunk`, `MatchLine`, `SearchResult`, `IndexStats`. |
| Public surface: `DependencyGraph`, index type, `Chunk`, `IndexStats`, `SearchResult`, plus `search`/`outline`/`plan`/`digest` modules | Confirmed — `engine/mod.rs` declares exactly this (index type is named `SembleIndex`). |
| ~7,900 lines / 19 modules | Measured 7,855 lines across the 19 `.rs` files under `engine/` — matches "roughly". |

No contradiction found. The package proceeds.

**One environment gap, now resolved.** This machine's `rustup stable` was
1.95.0; SPEC.md/CLAUDE.md pin `rust-version = "1.97"`, which `cargo` enforces
against the root package and would have failed AC1 outright. Ran
`rustup update stable`, which pulled 1.98.0 (released 2026-08-18). Toolchain
now satisfies the pin. Flagging this because it's a machine-state change
outside the repo, not something implied by the task.

**One behavioural note for later work packages, not a WP1 blocker.**
`engine/stats.rs::save_search_stats` — called from both `SembleIndex::search`
and `SembleIndex::find_related` — appends a JSON line to
`dirs::home_dir()/.semble/savings.jsonl` on every call, silently swallowing
any I/O error. That's a write outside the repository root on every search.
WP3's `context_search` tool constraint ("confine all filesystem access to the
repository root") will need to account for this — the do-not-refactor rule
means we cannot change engine code to stop it. Recording it now so WP3
doesn't discover it mid-package.

## Approach

### Crate shape

Package name `codemason-core` (so the default lib crate name is
`codemason_core`, matching SPEC T1.2's literal `codemason_core::Error` path)
with an explicit `[[bin]]` target named `codemason` at `src/bin/codemason.rs`
(SPEC's "distribution: single self-contained binary", CLI usage
`codemason run ...`). `src/bin/codemason.rs` in WP1 is a placeholder
`fn main() {}` — the real CLI arrives in WP2 T2.1. This split is a naming
inference from the spec text, flagged here for the gate rather than assumed
silently.

### `Cargo.toml`

```toml
[package]
name = "codemason-core"
version = "0.1.0"
edition = "2024"
rust-version = "1.97"
publish = false

[lints.rust]
unsafe_code = "deny"

[[bin]]
name = "codemason"
path = "src/bin/codemason.rs"

[features]
default = []
embeddings = ["dep:model2vec-rs", "dep:ndarray"]

[dependencies]
anyhow = "=1.0.104"
chrono = { version = "=0.4.45", default-features = false, features = ["std", "clock", "serde"] }
regex = "=1.13.1"
dirs = "=6.0.0"
ignore = "=0.4.33"
log = "=0.4.33"
once_cell = "=1.21.4"
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.151"
tree-sitter = "=0.25.10"
tree-sitter-c = "=0.23.4"
tree-sitter-c-sharp = "=0.23.5"
tree-sitter-cpp = "=0.23.4"
tree-sitter-css = "=0.25.0"
tree-sitter-go = "=0.23.4"
tree-sitter-html = "=0.23.2"
tree-sitter-java = "=0.23.5"
tree-sitter-javascript = "=0.23.1"
tree-sitter-kotlin-ng = "=1.1.0"
tree-sitter-php = "=0.23.11"
tree-sitter-python = "=0.23.6"
tree-sitter-ruby = "=0.23.1"
tree-sitter-rust = "=0.24.2"
tree-sitter-swift = "=0.7.3"
tree-sitter-typescript = "=0.23.2"
model2vec-rs = { version = "=0.2.1", optional = true }
ndarray = { version = "=0.15.6", optional = true }

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

Every version above is copied from the source project's own pins (its
`Cargo.toml` and workspace `[workspace.dependencies]`), not freshly
resolved — matching SPEC's "known-good set... do not resolve fresh ones."
`env_logger` is in the source crate's manifest but grep found zero use of it
inside `engine/` itself (only bare `log::` macro calls, which need the `log`
crate but not an initializer) — left out of WP1's dependency set since
nothing in the vendored code needs it; a later WP can add it if the runner
wants to initialize a logger.

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
profile = "default"
```

Copied from source verbatim — same reasoning applies (tracks current stable
rather than floor edition-2024 support).

### `.gitignore`

Standard Rust: `/target`, plus `old_source/` (so the reference tree never
enters git tracking) and a note that it's a local reference only.

### `src/lib.rs`

```rust
pub mod engine;
pub mod error;
pub mod index;

pub use engine::{Chunk, DependencyGraph, IndexStats, SearchResult};
pub use error::Error;
pub use index::Index;
```

### `src/error.rs`

`codemason_core::Error` as a `thiserror`-free (no new dependency) plain enum
or `enum Error { ... }` implementing `std::error::Error` + `Display` by
hand, OR pull in `thiserror` (already an approved dependency per SPEC WP3,
just not yet declared) — decision: add `thiserror = "=2.0.19"` now since
WP2/WP3 need it anyway and T1.2 explicitly calls for "this project's own
error type," which reads better with `thiserror`'s derive. Variants needed
for WP1's actual surface: `IndexBuild(anyhow::Error)`,
`EmbeddingsFeatureDisabled`. No reference to `harness_core::ExitCode` or
any type from `old_source` — exit-code mapping is CLI-layer, arrives in WP2.

### `src/engine/**`

Byte-for-byte copy of
`old_source/navex-harness-main/harness/crates/lib/context/src/engine/` into
`src/engine/`. No renames, no formatting pass, no clippy fixes even if
`cargo clippy` flags something. `ORIGIN.md` copied alongside (already lives
inside the source `engine/` folder, so it comes along with the directory
copy). The licence file is `LICENSE.semble` at the *source crate's* root,
not inside `engine/` — copied to `LICENSE-ENGINE` at the codemason crate
root per SPEC's file list, content unchanged (MIT, hunsang jo).

### `src/index.rs`

```rust
pub struct Index {
    inner: engine::SembleIndex,
    stats: BuildStats, // wraps engine::IndexStats + build_ms
}

impl Index {
    pub fn build(repo_root: &Path) -> Result<Self, Error> { ... } // encoder: None, times the call
    pub fn search(&self, query: &str, top_k: usize) -> Vec<SearchResult> { ... }
    pub fn chunks(&self) -> &[Chunk] { ... }
    pub fn graph(&self) -> &DependencyGraph { ... }
    pub fn stats(&self) -> &BuildStats { ... }

    #[cfg(feature = "embeddings")]
    pub fn find_related(&self, chunk: &Chunk, top_k: usize) -> Result<Vec<SearchResult>, Error> { ... }
    #[cfg(not(feature = "embeddings"))]
    pub fn find_related(&self, _chunk: &Chunk, _top_k: usize) -> Result<Vec<SearchResult>, Error> {
        Err(Error::EmbeddingsFeatureDisabled)
    }
}
```

`find_related` isn't in T1.3's named export list (`build, search, chunks,
graph, stats`), but AC6 ("the similarity call returns an error naming the
missing feature") is only testable if the wrapper exposes *something* that
reaches it, and it's the only call in the engine that needs the encoder.
Exposing it cfg-gated the way above is the minimal way to satisfy AC6
without adding a tool or contradicting T1.3's list — flagging the
discrepancy here rather than silently picking one reading.

## Test strategy, by acceptance criterion

- **AC1** `cargo build --release` — run directly, record pass/fail.
- **AC2** `cargo tree` — grep output for `model2vec`, `ndarray`, `openssl`,
  `git2`, `tokio`/`async-std`, and any `path`/`git` dependency line; must be
  empty on the default feature set.
- **AC3** `cargo build --release --features embeddings` — run directly.
- **AC4** `codemason index --stats`-equivalent: a `#[test]` in
  `src/index.rs` that builds an `Index` against
  `old_source/navex-harness-main/metric-measurement-service` (real C# repo,
  675 `.cs` files, already on disk) and asserts `total_chunks > 0` and
  `indexed_files > 0`. This test reads `old_source/` at test time only — it
  is not copied, committed, or depended on by the shipped crate.
- **AC5** Same fixture: `index.search("AgentSchedulerService", 5)` (a real
  type name from that repo) asserts the top result's `chunk.file_path` ends
  in `AgentSchedulerService.cs`.
- **AC6** Unit test built *without* the `embeddings` feature: call
  `find_related` and assert the error names the feature.
- **AC7** No test — a direct recursive diff (`diff -rq` /
  `Compare-Object`) of `src/engine/` against
  `old_source/navex-harness-main/harness/crates/lib/context/src/engine/`,
  run and reported as text output, not a script left in the tree.

## Risk flags

1. **Package/crate naming is an inference**, not a spec quote — flagged
   above, wants explicit sign-off at the gate.
2. **`thiserror` pulled in one package early** (WP3 territory) because
   T1.2's error type is awkward without it — flagged for sign-off.
3. **`stats.rs` writes outside the repo root** on every search call — noted
   for WP3, not fixed here (do-not-refactor).
4. **AC4/AC5 fixtures reach into `old_source/`** at test time. That
   directory is a local reference tree, not part of this repo's committed
   history — a test depending on it only works on this machine. Flagging
   so you can tell me whether to (a) accept that for WP1's own dev-loop
   verification only, since nothing under `src/` or `Cargo.toml` references
   it, or (b) copy a small fixture repo into this repo now instead. Default
   plan is (a): WP5 already owns building a proper fixture repo, and
   duplicating that early is scope creep.
5. **`old_source/` must never be committed.** It's already untracked per
   `git status`; the `.gitignore` entry above makes that durable rather than
   incidental.
