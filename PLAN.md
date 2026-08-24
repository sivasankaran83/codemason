# PLAN.md — WP5: Measure, containerise, accept

## Scope (restated)

The closing package. Three tasks, no new product code:

- **T5.1** Index cost measurement — a *decision* task. Run `codemason index
  --stats` against three real repositories of different sizes, including the
  largest available on this machine, cold and warm. Record chunk count and
  build duration. The numbers pick one of three paths: under ~5s on the
  largest → do nothing further (in-memory build stays); over ~15s →
  persistence becomes M2 scope; between → defer. Record the numbers and the
  decision in `README.md` either way.
- **T5.2** Container — multi-stage `Dockerfile`: slim Rust builder, slim
  Debian runtime carrying `git` and CA certificates (apt lists removed), no
  model weights. `git` is required at runtime because the binary shells out
  to it rather than linking a git library.
- **T5.3** Acceptance suite — consolidate every WP1–4 AC into an automated
  test where automatable, plus two items the earlier packages' own tests
  don't cover: two concurrent `codemason run` invocations against two
  independent repo clones both succeeding with independent logs and
  branches, and one dedicated full-successful-run smoke test (exit 0,
  branch, commit, valid single-JSON stdout).

WP5's own acceptance criteria (AC1–AC4) map onto these three tasks 1:1, plus
AC3 (`cargo test --release` passes the full suite on Windows 10) as the
umbrella check over everything T5.3 adds and everything already committed by
WP1–4.

No named gate for WP5 in SPEC.md's per-package callouts (WP1/AC7 and
WP3/AC7 were the two called out) — all four ACs below are equally
load-bearing this package.

## Feasibility findings

All confirmed by direct inspection this session, not assumed:

**Environment.**
- `cargo build --release` succeeds clean (1m19s, LTO fat + codegen-units=1).
  A prior release binary already existed; rebuilt to confirm current source
  matches.
- **Docker is not installed and no WSL Linux distribution is present**
  (`docker` absent from PATH, no Docker Desktop under `Program Files`, no
  service, `wsl --list --verbose` returns only the launcher usage text — no
  distro registered). T5.2's Dockerfile can be written correctly by
  inspection against the spec's shape, but **building and running it cannot
  be exercised in this session**. WP5/AC2 will be reported `not run` with
  this reason; the Dockerfile itself is still a real deliverable for a human
  to build where Docker exists.
- `git version 2.52.0.windows.1` present (already used by WP4's `repo.rs`).

**Real repositories available for T5.1**, searched beyond `C:\repo` (user
profile source folders, `.gemini/history` — nothing larger found) —
`C:\repo\CodeFabric` is the largest real repository on this machine:

| repo | files indexed | chunks | cold ms | warm ms |
|---|---|---|---|---|
| `buildsmith` | 18 | 95 | 329 | 53 |
| `CapaFabric` | 66 | 121 | 1198 | 126 |
| `CodeFabric` | 214 | 879 | 4503 | 1348 |

("Cold" here means first read this session, before the same repo's files
were touched again — not a true dropped OS cache, which needs elevated
tooling unavailable here. Flagged as a methodology caveat, not hidden.)
`C:\repo\AgentCore` was also tried and correctly rejected — `.csproj`/`.sln`
scaffolding only, no source files the engine's grammars cover, matching the
"No supported files found" error path rather than a bug.

Result previews the decision: even the largest available repo builds in
4.5s cold. That's inside "under ~5s" — the likely T5.1 outcome is "build
in-memory per run, do nothing further," but the actual recorded run during
Implement is what goes in the README, not this preview.

**Existing automated coverage** (from WP2–4's own Test stages, already
committed, 1,819 lines across `tests/*.rs` plus unit tests in ~10 `src/`
modules) already satisfies, by direct read of test names and bodies:

- WP1: AC4, AC5, AC6 (`src/index.rs`, `src/tools/context.rs`, both keyed off
  the on-disk-only `old_source` C# fixture).
- WP2: AC1, AC2, AC3, AC5, AC6, AC7, AC8, AC9 (`tests/help.rs`,
  `tests/index_stats.rs`, `tests/models_allowlist.rs`, `tests/gating.rs`).
- WP3: AC1, AC2, AC3, AC4, AC5, AC6, AC7, AC8, AC9 (`tests/llm_client.rs`,
  `src/text.rs`, `src/tools/context.rs`, `src/tools/mod.rs`,
  `tests/loop_.rs`, `tests/event_log.rs`).
- WP4: AC1, AC2, AC3, AC4, AC5, AC6, AC7, AC8, AC9 (`src/tools/fs.rs`,
  `src/tools/exec.rs`, `tests/writes_commands_git_budget.rs`,
  `tests/provider_error.rs`) — including AC5's process-tree kill, which
  already exists (`timeout_kills_the_full_process_tree_not_just_the_shell`)
  and AC9's per-exit-path JSON check, satisfied collectively across the
  existing files' calls to `common::assert_single_json_report` at 0, 1, 2,
  3, 4 and 5.

**Gaps this package fills:**

- WP1/AC1 — inherent in `cargo build`/`cargo test` succeeding at all; no
  separate test needed or meaningful to write.
- WP1/AC2 — **currently does not hold**: `cargo tree` shows `model2vec-rs`
  and `ndarray` in the default (no `--features embeddings`) dependency tree.
  Confirmed today with `cargo tree -e normal --depth 0 -i model2vec-rs`. Root
  cause, read directly from `Cargo.toml`'s existing comment and confirmed
  against the vendored source: `src/engine/mod.rs` declares `pub mod
  encoder;` unconditionally (no `cfg`), and `src/engine/encoder.rs`
  unconditionally imports both crates — so making them `optional = true`
  would break the *default* `cargo build` (AC1) instead, and editing
  `engine/mod.rs` or `encoder.rs` to add a cfg gate is forbidden by
  CLAUDE.md's do-not-refactor rule. WP1 chose to keep AC1 green over AC2.
  **Resolved with the developer this session**: the WP5 acceptance test
  asserts the amended contract WP1 actually shipped — no `openssl`, `git2`,
  async-runtime, or out-of-repo path/git dependency in the tree, and
  embedding *functionality* (not the crate's presence) is feature-gated,
  which the existing `ac6_similarity_call_names_the_missing_feature...` unit
  test already enforces. AC2 is reported `pass — amended contract, see PLAN`
  in the WP5 table, not silently reworded in SPEC.md.
- WP1/AC7 — no test exists yet. **The "supplied source tree" is
  `old_source/navex-harness-main/harness/crates/lib/context/src/engine`** —
  identified by searching `old_source` for the engine's module names
  (`bm25.rs`, `chunking.rs`, `outline.rs` all found there) and confirmed with
  `diff -rq src/engine old_source/.../context/src/engine`, which returned
  **zero output** — the trees, including `ORIGIN.md`, are byte-identical.
  (The `ORIGIN.md` text describes a *different* upstream project, "semble_rs"
  — that reads oddly out of context but is not a WP1 error: it's exactly
  what the supplied tree carried, correctly preserved verbatim per the Must
  list's "carry the ORIGIN.md... that accompanies the supplied engine
  source." Diff confirms it, so no further action.) A new test formalises
  this diff so it stays enforced going forward.
- WP1/AC3 (`cargo build --release --features embeddings` also succeeds) —
  no automated check exists. Building this into `cargo test --release` as a
  nested full release rebuild would double the suite's wall-clock time
  (LTO fat + codegen-units=1 is already the dominant cost, confirmed by the
  1m19s default build just now) every single run, for a criterion that only
  needs checking when `Cargo.toml`'s dependency shape changes. **Default
  call: verify it once by hand this package** (`cargo build --release
  --features embeddings`) and report the result directly in the AC table,
  rather than adding a permanent nested-build test. Flagged as a risk below
  in case that trade-off isn't the one wanted.
- WP2/AC4 (`models --check` against a *live* catalogue) — needs a real
  provider endpoint and API key; not something an offline automated suite
  can exercise. Reported `not run — requires a live provider credential`,
  consistent with how WP2's own report presumably left it (no test named for
  it exists in `tests/`).
- T5.3's two named additions — concurrency and a dedicated full-run smoke
  test — genuinely new, written this package.

No contradiction with SPEC.md's Current State section beyond the two items
above, both resolved (one with the developer, one by direct diff). The
package proceeds.

## Approach

### T5.1 — measurement (no code; README only)

Re-run the three `codemason index --stats` invocations cleanly during
Implement (the numbers above are a preview from feasibility, not the
recorded run), cold and warm as already demonstrated, and write a new
`## Index cost measurement` section into `README.md`: a table (repo, files,
chunks, cold ms, warm ms) plus the decision sentence with reasoning,
matching T5.1's three-way rule. Note the cold-cache methodology caveat
inline rather than overstating rigor.

### T5.2 — `Dockerfile` (new) and `.dockerignore` (new)

```dockerfile
# syntax=docker/dockerfile:1
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY LICENSE-ENGINE ./
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/codemason /usr/local/bin/codemason
ENTRYPOINT ["/usr/local/bin/codemason"]
```

`.dockerignore` excludes `/target`, `old_source/`, `.git`, `tests/`,
`*.md` except `README.md` isn't needed in-image at all — excluded too —
and the scratch/log directories, so the build context stays small (no LTO
build artifacts, no local-only reference tree get sent to the daemon).

No model weights enter the image — nothing in the runtime stage references
`model2vec-rs`'s data files, matching the BM25-only design intent even
though the crate itself compiles into the binary (T5.1's WP1/AC2 finding
above; irrelevant to image contents since no weights are ever downloaded or
shipped regardless of that crate's presence in the dependency tree).

**Verification is capped by environment, not by the file's correctness**:
this session has no Docker/WSL to run `docker build`/`docker run` against.
The Dockerfile is written to match the spec's shape exactly and reviewed by
inspection; AC2's build/run/size-measurement is reported `not run` with that
reason, once, honestly — not claimed as passing.

### T5.3 — new test files

**`tests/wp1_engine.rs`** (new):

- `ac2_default_tree_excludes_forbidden_crates_and_gates_embeddings` — shells
  `cargo tree -e normal` via `Command::new(env!("CARGO"))`, asserts no line
  matches `openssl`, `git2`, `tokio`, `async-std`; asserts no `(*)`-free path
  or git source annotation outside the workspace. Doc comment names the
  WP1/AC2 deviation and links to this file for the reasoning, rather than
  re-explaining it inline at length.
- `ac7_engine_tree_matches_the_supplied_source_byte_for_byte` — walks
  `src/engine/` and
  `old_source/navex-harness-main/harness/crates/lib/context/src/engine`,
  asserts identical relative file sets and identical bytes per file. Skips
  (with an `eprintln!`, matching `tests/index_stats.rs`'s existing
  convention) if `old_source` isn't present on this checkout.

**`tests/full_run_and_concurrency.rs`** (new):

- `wp5_ac4_two_concurrent_runs_against_two_clones_succeed_independently` —
  two temp repo clones (`common::temp_dir` + `common::init_git_repo`), two
  independent `RoutedStubServer`s (avoids any cross-talk, cleanest read on
  "independent"), two `codemason run` children spawned via
  `Command::spawn()` before either is waited on, then both `.wait_with_
  output()`d. Asserts both exit 0, both produced a distinct branch name and
  a distinct commit SHA, both stdout reports parse via
  `assert_single_json_report`, and neither's log/branch/commit collided with
  the other's.
- `wp5_full_successful_run_exit0_branch_commit_valid_json` — single-clone
  version of the same shape, asserting the four things T5.3 names
  explicitly by name in one place for direct traceability, even though
  `writes_commands_git_budget.rs::ac6_run_commits_when_something_changed`
  already exercises materially the same path under a WP4-numbered test.

No changes to any `src/` file — WP5 is measurement, packaging and test
consolidation only, not new product behaviour.

### `README.md` additions

- `## Index cost measurement` (T5.1 table + decision, see above).
- `## Container` — build/run instructions (`docker build -t codemason .`,
  example `docker run --rm -v <repo>:/repo codemason run --repo /repo
  --task ...`), image size once measured, and a one-line note that AC2's
  execution was not verified in this development environment for the reason
  above (so the gap is visible to whoever reads this next, not just to
  whoever reads the WP5 report).
- `## Status` line updated: WP5 added to the completed-packages list, with
  the AC2 caveat repeated in the same breath so the top-level status doesn't
  overstate it.

## Test strategy, by acceptance criterion

- **WP5/AC1** — manual: re-run the three `index --stats` invocations,
  transcribe into `README.md`. Not a `cargo test` assertion (a build-speed
  number isn't a pass/fail unit test; the *decision* is the deliverable).
- **WP5/AC2** — manual, capped by environment: `Dockerfile` written and
  reviewed by inspection; `docker build`/`docker run`/image-size cannot run
  here. Reported `not run` with reason.
- **WP5/AC3** — `cargo test --release` run in full at Verify. Everything
  above (existing WP1–4 coverage plus the two new files) is what this
  criterion actually measures.
- **WP5/AC4** — `wp5_ac4_two_concurrent_runs_against_two_clones_succeed_
  independently`, run directly and also as part of the full suite.

Consolidated WP1–4 ACs ride on the existing tests enumerated in Feasibility
above, plus the two new `tests/wp1_engine.rs` cases for WP1/AC2 and AC7, plus
a manual, once-off check for WP1/AC3 (see risk flag 2).

## Risk flags

1. **AC2 (container) cannot be executed or verified in this session** — no
   Docker, no WSL distro on this machine. The Dockerfile is a real,
   reviewed deliverable; its build/run success is not. If a human runs it
   on a Docker-capable machine and it fails, that's new information this
   session couldn't surface.
2. **WP1/AC3 (embeddings feature build) verified manually, not via a
   permanent test** — chosen to avoid doubling `cargo test --release`'s
   wall-clock every run for a criterion that only changes when dependency
   shape changes. If the project wants it enforced continuously instead
   (e.g. a CI-only `#[ignore]`d test invoked separately from the default
   suite), that's a small addition on top of this plan, flagged here rather
   than assumed.
3. **WP1/AC2's amended-contract test is a policy call, not a spec literal**
   — resolved with the developer this session (see Feasibility above), but
   worth restating at the gate since it changes what "AC2 passes" means for
   every future run of the suite, permanently, not just for this report.
4. **Cold-cache measurement in T5.1 is best-effort** — "first touch this
   session" stands in for a true dropped OS cache, which needs elevated
   tooling not available here. The README will say this plainly rather than
   implying a rigor the numbers don't have.
5. **`CodeFabric` (the largest available repo) mixes several languages
   including large non-source directories** (a Rust `target/`-shaped
   subtree was excluded by hand during feasibility's file counting, not by
   the engine's own gitignore-aware walker, which is what actually runs
   during `index --stats`) — the recorded chunk count reflects whatever the
   walker's own gitignore/default-exclusion rules produce, which may differ
   slightly from the manually-filtered preview count above. The Implement-
   stage numbers, not this preview, are what goes in the README.
