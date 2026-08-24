# PLAN.md — WP4: Writes, commands, git, budget

## Scope (restated)

Everything that changes state. Four tasks:

- **T4.1** `write_file`: whole-file replacement, creates parent dirs/file if
  absent, applies WP3's line-ending/BOM restoration against whatever
  convention the file currently has on disk. Rejects (as a tool-result error,
  never a failed run): content over 500 KB, a path outside the repo root,
  content that looks elided, or any attempt under `--dry-run`.
- **T4.2** `run_command`: runs at the repo root via the platform shell,
  default timeout 120 s / max 900 s, kills the **full process tree** on
  timeout, captures interleaved stdout+stderr capped at the last 100 KB,
  always returns the real exit code as a normal (non-error) result, refuses
  under `--dry-run`. No allowlist — the container + disposable repo copy is
  the isolation boundary (already documented in README.md's Safety section).
- **T4.3** Git: one module owns every repository operation. Preflight
  (git-worktree + clean check, refuse dirty unless `--dry-run`, exit 1 before
  any API call), branch create/checkout, postflight (stage, commit only if
  something changed, capture SHA + changed paths).
- **T4.4** Budget/ceiling/report: check token *and* USD budget immediately
  before each API call, never after; refuse to start at budget `0`/`<=0`;
  enforce the iteration ceiling (unenforced since WP3); on breach, log, break,
  commit partial work, exit 2 or 3; emit exactly one JSON report object on
  stdout for every exit path of `run`, diagnostics to stderr.

Gate: none named specifically for WP4 in SPEC.md's per-package callouts (that
was WP1/AC7 and WP3/AC7) — all nine ACs below are equally load-bearing this
package.

## Feasibility findings

All WP4 target files from SPEC.md's file list exist in the state WP3 left
them, confirmed by direct read, not assumption:

- `src/tools/mod.rs` — registry already declares all six schemas (`write_file`:
  `path`+`content`; `run_command`: `command`+`timeout_seconds`); `dispatch`
  currently routes both to a stub `ToolOutcome::Error("... not available until
  WP4")`. This package swaps the two stub closures for real handlers; no
  schema change.
- `src/tools/fs.rs` — `read_file`/`list_files` are real; file header comment
  already says `write_file` lands here and `run_command` in a new `exec.rs`.
- `src/loop.rs` — `LoopConfig.max_iterations` exists but is
  `#[allow(dead_code)]`, read by nothing (WP3's own comment: "T4.4 owns...
  this field is not read by `run` yet"). `LoopExit` has two variants
  (`Completed`, `ProviderError`); WP4 adds `BudgetExceeded` and
  `MaxIterationsExceeded`. The per-tool-call `ToolContext` is built inline at
  `src/loop.rs:144`; needs a `dry_run` field threaded from `LoopConfig`.
- `src/error.rs` — `ProviderExhausted`/`ProviderRequest`/`PathEscapesRepo`
  already present from WP3; no git or budget variants yet.
- `src/cli.rs` — `RunArgs` already carries `budget_tokens: u64`,
  `budget_usd: Option<f64>`, `max_iterations: u32`, `branch: Option<String>`,
  `dry_run: bool` — every flag T4.4/T4.3 need is already parsed. No CLI
  surface change needed this package.
- `src/bin/codemason.rs`'s `run_cmd` currently: parses args → resolves
  config/model → resolves credentials → fetches catalogue → gates → opens the
  event log → builds the index → constructs the client → runs the loop →
  `eprintln!`s the summary. No git preflight, no branch, no postflight commit,
  no budget threading, no stdout JSON report — this package's real surface.
- `src/repo.rs` does not exist. `src/tools/exec.rs` does not exist. Both
  expected — this package creates them.
- No new crate is needed. Git operations shell out to the `git` CLI (present:
  `git version 2.52.0.windows.1`) via `std::process::Command`, matching the
  existing pattern (`gating.rs` shells to nothing but the client already uses
  `ureq`; git needs no client library per the Must Not list). Process-tree
  kill on Windows shells to `taskkill /PID <pid> /T /F`, also no new
  dependency. Neither needs `unsafe_code`, which stays denied.
- `git status --porcelain=v1` on this repo returns nothing (clean) — the
  precondition for starting work is met.

No contradiction with SPEC.md's Current State section. The package proceeds.

## Ambiguity resolved at kickoff

**Budget refusal threshold (asked, not self-resolved).** T4.4's prose says
"refuse to start when the budget is at or below zero," but AC8 says
`--budget-tokens 1` exits 2 with **zero** API calls — and with 0 tokens spent
before the first call, a strict `spent >= budget` check lets call #1 through
(0 < 1); the breach is only visible before call #2. Asked the developer:
**strict zero-only** confirmed. `refuse to start` fires only when
`budget_tokens == 0` or `budget_usd <= Some(0.0)`. AC8's "zero calls" case is
tested at `--budget-tokens 0`, not `1`; a separate test at a small nonzero
budget (e.g. `1`) demonstrates one call happens, the breach is caught before
call #2, and partial work is still committed — same protective behavior AC8
describes, at the value the spec's own prose implies rather than the value
its AC literally names. Flagged as a deviation from AC8's literal wording,
not from its intent.

## Other inferences (flagged, not asked — same treatment WP2/WP3 gave
similar unspecified-but-not-contradictory details)

1. **Default branch name.** `--branch` is optional with no spec-stated
   default. Since `run_id` (UUID v7) is already generated for the event log,
   reused as `codemason/<run_id>` when `--branch` is omitted — unique per run,
   sortable, namespaced so it can't collide with a human branch.
2. **`.agent/` excluded from the git preflight clean-check and from the
   postflight `git add`.** `.agent/` is this tool's own working area (already
   excluded from `list_files` and the default `--log` path lives under it).
   Without exclusion, a prior run's leftover `.agent/log/*.jsonl` would make
   every subsequent run's preflight see a dirty tree and refuse to start —
   an obvious operational trap the spec almost certainly doesn't intend.
   Implemented via git pathspec exclusion (`-- . ':!.agent'`) on both the
   status check and `git add -A`, so run logs are never part of a commit and
   never block the next run.
3. **Postflight commit runs after the loop for every outcome that reached the
   loop** (`Completed`, `BudgetExceeded`, `MaxIterationsExceeded`, and
   `ProviderError`), not only the two breach cases the Must list names by
   name. Rationale: "partial work is often useful" is stated as a general
   principle, and a provider error after several successful tool calls is
   exactly a case where discarding on-disk changes would be needlessly
   destructive. `--dry-run` skips all git mutation regardless of outcome —
   no branch is created, nothing is staged or committed — so AC3's "leaves
   the filesystem untouched" holds for `.git` state too, not only tracked
   files.
4. **`RunReport.status` string values** — not named in SPEC.md. Using:
   `completed`, `budget_exceeded`, `max_iterations_exceeded`, `model_gated`,
   `provider_error`, `unrecoverable_error`. One value per `ExitCode` variant
   reachable from `run`.
5. **Every exit path of `run_cmd` emits the JSON report**, per AC9's "every
   exit path emits a matching `exit_code`" read together with "yields a file
   parsing as one JSON object." `run_id` is generated first, before argument
   validation, so even the earliest failure (bad `--task` file, missing
   credential, config error, dirty worktree, model gated) can still populate
   `run_id`, `status`, `exit_code`, `duration_ms`, and leave the rest
   (`branch`, `commit`, `index`, `models_used`, `totals`) null/empty rather
   than omitted. `models`/`index` subcommands are unaffected — AC9 only
   concerns `run`.
6. **Unix process-tree kill for `run_command`** is implemented (spawn via
   `setsid sh -c '<command>'`, kill via shelled-out `kill -TERM -<pgid>` then
   `-KILL`) so the container deploy target isn't left with a known-broken
   path, but WP4's acceptance testing happens on Windows only per SPEC.md's
   constraints table — the Unix path is unverified this package and flagged
   as a risk, not claimed as tested.

## Approach

### `src/error.rs` additions

```rust
NotAGitWorktree { path: PathBuf },
DirtyWorktree { path: PathBuf },
GitCommand { args: String, source: anyhow::Error },   // non-zero exit from a git invocation we require to succeed
```

No git2/libgit2 types anywhere — every variant carries only strings/paths, per
the existing rule (T1.2/T2/T3 carried forward).

### `src/repo.rs` (new)

```rust
pub struct CommitInfo { pub sha: String, pub files_changed: Vec<String> }

pub fn preflight(repo_root: &Path, dry_run: bool) -> Result<(), Error>;
// `git rev-parse --is-inside-work-tree` (not a worktree -> NotAGitWorktree);
// `git status --porcelain -- . ':!.agent'` non-empty and !dry_run -> DirtyWorktree.
// dry_run short-circuits both checks (Ok(()) unconditionally).

pub fn create_branch(repo_root: &Path, name: &str) -> Result<(), Error>;
// `git checkout -b <name>`; non-zero exit -> GitCommand.

pub fn commit_all(repo_root: &Path, message: &str) -> Result<Option<CommitInfo>, Error>;
// `git add -A -- . ':!.agent'`;
// `git diff --cached --name-only` -> files_changed (empty means nothing to commit -> Ok(None));
// `git commit -m <message>` only if files_changed is non-empty;
// `git rev-parse HEAD` -> sha.
```

Every function shells to `git` via `std::process::Command::new("git").current_dir(repo_root)`,
matching the Must's "shell out to the git CLI" and Must Not's "git2/libgit2
must not be linked." A non-zero exit from a git invocation this module
requires to succeed (checkout, commit, rev-parse) maps to `Error::GitCommand`
with stderr captured — never a panic.

### `src/tools/mod.rs`

- Add `pub mod exec;`.
- `ToolContext` gains `pub dry_run: bool`.
- `dispatch`'s `"write_file"` arm calls `fs::write_file(ctx, &a.path, &a.content)`.
- `dispatch`'s `"run_command"` arm calls
  `exec::run_command(ctx, &a.command, a.timeout_seconds)`.
- Drop the now-unnecessary `#[allow(dead_code)]` on `WriteFileArgs`/`RunCommandArgs`
  fields, since both are read for real.

### `src/tools/fs.rs` — `write_file`

```rust
pub fn write_file(ctx: &ToolContext, path: &str, content: &str) -> ToolOutcome
```

- `text::to_repo_relative`-equivalent resolution, but tolerant of a
  not-yet-existing file: resolve the parent directory through
  `to_repo_relative`, then join the final component (the existing helper
  canonicalizes the full candidate, which fails for a file that doesn't exist
  yet — `write_file` needs a variant that only requires the parent to exist
  and be inside the root; added as `text::to_repo_relative_for_write`,
  reusing `normalize_slashes` + a parent-only canonicalize-and-prefix-check,
  same traversal-rejection logic as the read path).
- `content.len() > 500 * 1024` → `ToolOutcome::Error` naming the limit, file
  untouched.
- `ctx.dry_run` → `ToolOutcome::Error("write_file: simulated under --dry-run, no write performed")`,
  checked before touching the filesystem.
- If the file exists: read its current bytes, run `text::read_for_model` to
  get its current `(line_ending, had_bom)` and LF-normalized existing
  content; run `text::looks_elided(existing, content)` — true →
  `ToolOutcome::Error` instructing the model to supply the complete file,
  file untouched. Restore convention via `text::restore_for_write(content, line_ending, had_bom)`.
- If the file does not exist: create parent directories
  (`std::fs::create_dir_all`), write `content` as plain UTF-8 bytes, no BOM
  — there is no prior convention to preserve for a brand-new file.
- Write via a temp-file-plus-rename in the same directory (`std::fs::write`
  is adequate here — whole-file replacement, no partial-write concern beyond
  what any tool run already carries) then return
  `ToolOutcome::Ok("wrote N bytes to {path}")`.

### `src/tools/exec.rs` (new) — `run_command`

```rust
pub fn run_command(ctx: &ToolContext, command: &str, timeout_seconds: i64) -> ToolOutcome
```

- `ctx.dry_run` → immediate `ToolOutcome::Error("run_command: simulated under --dry-run, not executed")`.
- Timeout: `0` → default 120 s (existing 0-means-default convention); clamp
  the requested value to `[1, 900]` s otherwise.
- Spawn platform shell at `ctx.repo_root` with piped stdout+stderr:
  - Windows: `cmd /C <command>`.
  - Unix (`cfg(unix)`, best-effort — see inference 6): `setsid sh -c <command>`,
    recording the child PID as the process-group ID `setsid` gives itself.
- Two reader threads drain stdout/stderr into one `Arc<Mutex<Vec<u8>>>`
  capped at 100 KB, dropping from the front (keeping the **last** 100 KB) and
  setting a `truncated` flag once the cap is first exceeded — matches
  "build errors appear at the end."
- Main thread polls `child.try_wait()` in a short sleep loop against a
  deadline; on timeout:
  - Windows: shell out `taskkill /PID <pid> /T /F`.
  - Unix: shell out `kill -TERM -<pgid>`, brief grace period, then
    `kill -KILL -<pgid>` if still alive.
  - Reap the child, return `ToolOutcome::Error` naming the timeout with
    whatever output was captured appended — a killed command has no real
    exit code to report.
- On normal exit: format `ToolOutcome::Ok` (even for non-zero exit — "a
  non-zero exit is a normal result, not a tool failure") containing the exit
  code, a truncation notice if the cap fired, and the captured output.

### `src/loop.rs`

- `LoopConfig` gains `budget_tokens: u64`, `budget_usd: Option<f64>`,
  `dry_run: bool`; `max_iterations` loses `#[allow(dead_code)]` — it's read
  now.
- `LoopExit` gains `BudgetExceeded { iterations: u32 }` and
  `MaxIterationsExceeded { iterations: u32 }`.
- Before the loop's first iteration and before every subsequent
  `client.complete` call: `budget_breached(cfg, &ledger)` —
  `cfg.budget_tokens == 0 || cfg.budget_usd.is_some_and(|b| b <= 0.0)` (start
  refusal, strict-zero per the resolved ambiguity) **or**
  `ledger.total_tokens() >= cfg.budget_tokens` **or**
  `cfg.budget_usd.is_some_and(|b| ledger.total_cost() >= b)` (breach from
  accumulated usage). On breach: log `budget_exceeded`, return
  `LoopExit::BudgetExceeded { iterations }` without making the call.
- Same site, iteration ceiling: `iterations >= cfg.max_iterations` (checked
  before incrementing for the call about to be made) → log
  `max_iterations_exceeded`, return `LoopExit::MaxIterationsExceeded { iterations }`.
- `ToolContext` construction at the per-call-site gains `dry_run: cfg.dry_run`.
- `UsageLedger` (in `src/llm/mod.rs`) gains `total_tokens(&self) -> u64` and
  `total_cost(&self) -> f64`, summing `Totals` across every model — needed by
  the two breach checks above and by the final report's `totals` field.

### `src/bin/codemason.rs` — `run_cmd` restructure

1. `let run_id = Uuid::now_v7();` and `let start = std::time::Instant::now();`
   first, before argument parsing — every subsequent return path can report
   `run_id` and `duration_ms`.
2. A local, mutable `RunReport` (new `src/report.rs`, `Serialize`) is threaded
   through the function and updated as more becomes known; a small
   `finish(report: RunReport, code: ExitCode) -> ExitCode` helper sets
   `status`/`exit_code`/`duration_ms`, serializes with `serde_json::to_string`
   (single line — "exactly one JSON object on stdout, nothing else"), writes
   it via `text::write_stdout`, and returns `code`. Every existing
   `eprintln!("error: ..."); return ExitCode::X;` pair becomes
   `eprintln!("error: ..."); return finish(report, ExitCode::X);` — diagnostics
   stay on stderr, unchanged; only the previously-bare returns gain the
   stdout report.
3. New step, immediately after argument parsing and before config
   resolution: `repo::preflight(&args.repo, args.dry_run)` — failure exits 1
   via `finish`, before any network call, satisfying AC7.
4. Existing config/credential/gating/index sequence is unchanged in order,
   each updating `report` fields as they succeed (`report.index = Some(...)`
   after `Index::build`, etc.).
5. After the index builds and before constructing the loop: if `!args.dry_run`,
   `let branch = args.branch.clone().unwrap_or_else(|| format!("codemason/{run_id}"));`
   then `repo::create_branch(&args.repo, &branch)?`; `report.branch = Some(branch)`.
   Under `--dry-run`, `report.branch` stays `None` and no branch is created.
6. `LoopConfig` gains the three new fields from `args`.
7. After `run_loop` returns, **unconditionally** (unless `--dry-run`):
   `repo::commit_all(&args.repo, &commit_message)?` — a fixed, non-templated
   message such as `"codemason: {task}"` truncated to a reasonable length is
   sufficient; not spec'd, low-risk. Populate `report.commit`,
   `report.files_changed` from the `Option<CommitInfo>` (both stay
   empty/`None` when nothing changed).
8. `report.iterations`, `report.models_used = vec![model_id]`,
   `report.totals` from `ledger.total_tokens()/total_cost()`, `report.log_path`
   are set from values already in scope. Exit code follows `LoopExit`:
   `Completed`→0, `BudgetExceeded`→2, `MaxIterationsExceeded`→3,
   `ProviderError`→5.

### `src/text.rs` addition

```rust
pub fn to_repo_relative_for_write(repo_root: &Path, raw: &str) -> Result<PathBuf, Error>;
```

Same traversal-rejection contract as `to_repo_relative`, but canonicalizes
only the parent directory (which must exist and resolve inside the root) and
joins the final path component uncanonicalized — the target file itself is
allowed not to exist yet. `read_file`/`list_files`/`context_*` keep using the
existing `to_repo_relative`, unchanged.

## Test strategy, by acceptance criterion

- **AC1** `src/tools/fs.rs` unit tests: write to a CRLF fixture with new LF
  content → file bytes still CRLF-terminated (`fs::read` verifies), `git diff`
  of a real fixture repo shows a minimal diff (line-content only, no
  line-ending churn); a BOM fixture retains its BOM after a write.
- **AC2** `src/tools/fs.rs` unit tests: elided content (existing large,
  new content under 50% length with a marker) → `ToolOutcome::Error`, file
  bytes unchanged on disk; a >500 KB content string → `ToolOutcome::Error`,
  no file created.
- **AC3** `tests/` integration test: `codemason run --dry-run` against a
  fixture repo with a task that would trigger a write; assert the working
  tree and `.git` refs are byte-identical before/after (no branch created,
  no commit), and the process still exits reflecting `LoopExit::Completed`
  (0) via a stub trace that terminates normally.
- **AC4** `src/tools/exec.rs` unit tests: a command producing >100 KB of
  output → result contains a truncation notice and exactly the last 100 KB;
  a command that `exit 1`s → `ToolOutcome::Ok` containing `exit_code: 1` (not
  `Error`).
- **AC5** `src/tools/exec.rs` test (Windows-only, matching the constraints
  table's primary verify platform): spawn a command that itself spawns a
  detached child holding a file lock (e.g. `cmd /C "start /B ... & timeout ..."`
  or a small helper batch script) with a timeout shorter than the child's
  lifetime; assert the file lock is released (the file is deletable) once
  `run_command` returns, proving the tree — not just the immediate shell —
  was killed.
- **AC6** `tests/`: a fixture repo with one commit succeeds end to end
  against a stub trace that calls `write_file` once — assert exactly one new
  commit exists (`git log --oneline` count) containing the expected file; a
  second run with a stub trace that makes no tool calls produces no new
  commit.
- **AC7** `src/repo.rs` unit test: `preflight` against a repo with an
  uncommitted change and `dry_run: false` → `Err(DirtyWorktree)`; integration
  test: `codemason run` (no `--dry-run`) against a dirty fixture exits 1 and
  the stub server (if any) recorded zero requests.
- **AC8** `tests/loop_.rs` / integration: `--budget-tokens 0` → `run_loop`
  returns `BudgetExceeded` at 0 iterations, ledger empty, zero stub requests
  (per the resolved ambiguity, tested at `0` not `1` — see above); a stub
  trace whose first response reports usage already exceeding a small
  `--budget-tokens` value → exactly one call made, `BudgetExceeded` returned,
  and (via the full `codemason run` integration path) a commit exists holding
  whatever the one completed tool call changed; `--max-iterations 2` against
  a stub trace with 3+ available turns → `MaxIterationsExceeded` after
  exactly 2 calls, partial commit present.
- **AC9** integration test: `codemason run ... > out.json 2> err.log` against
  a normal-completion stub — `out.json` parses as exactly one JSON object,
  `err.log` contains the human-readable summary/diagnostics; the same
  assertion repeated with `--verbose` shows stdout unchanged; one test per
  distinct exit path (dirty worktree→1, budget-tokens 0→2, max-iterations→3,
  model-gated→4, provider exhausted→5) each asserts stdout parses as one
  object whose `exit_code` field matches the process's actual exit code.

## Risk flags

1. **Budget refusal semantics deviate from AC8's literal "1"** — resolved
   with the developer this session (see Ambiguity section); documenting again
   here because it changes what the AC8 test actually exercises versus what
   the spec's table literally says.
2. **Unix process-tree kill (`setsid`/`kill -TERM -pgid`) is unverified** —
   Windows 10 is this project's only verify platform; the Unix path is
   written for the container deploy target but AC5 is only exercised on
   Windows this package.
3. **`.agent/` excluded from git status/add via pathspec** is an inference
   (see Other inferences #2) — if a real target repo already tracks a
   `.agent/` path for its own unrelated reasons, this exclusion would hide
   it from both the dirty-check and the commit, which could surprise an
   operator. Narrow and unlikely, flagged for visibility.
4. **Default branch name `codemason/<run_id>`** and the **commit message
   format** are both inferences with no basis in SPEC.md beyond "create and
   checkout the branch" / "commit... if something changed" — reasonable but
   not spec'd, flagged for sign-off same as WP2/WP3 flagged their own
   naming inferences.
5. **Postflight commit on `ProviderError`, not only budget/iteration breach**
   — Must list names only budget/iteration breach explicitly; extending the
   same "commit partial work" treatment to a provider error after partial
   tool-call progress is my reading of the stated principle, not a literal
   requirement. If wrong, `repo.rs`'s `commit_all` call site in
   `bin/codemason.rs` is the one place to gate it differently.
6. **`write_file`'s new `to_repo_relative_for_write` duplicates most of
   `to_repo_relative`** rather than generalizing the existing function with a
   `must_exist: bool` parameter — chosen to avoid touching `text.rs`'s
   already-tested read-path function and its five passing WP3 unit tests;
   flagged in case a single parameterized function is preferred instead.
