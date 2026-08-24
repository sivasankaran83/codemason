# Model-Agnostic Codebase Agent Runner — Milestone 1

**Execution mode:** AIDLC, Claude Code with Sonnet, one work package per session.
**Supersedes:** earlier drafts that assumed greenfield and free-running execution.

---

## Why

An agent that runs unattended against many repositories in parallel needs two
things: a client that drives a model directly over HTTP, and independence from
any one model vendor.

Vendor independence raises the stakes on retrieval. A strong model compensates
for weak search by reading widely and recovering; a cheap model given plain grep
burns iterations and hits the budget cap without finishing. Harvesting a proven
AST search engine is therefore not an optimisation — it is what makes cheap
execution viable at all.

M1 answers one question: does a minimal tool-calling loop, given good AST-aware
retrieval and a curated set of tool-capable models, produce correct committed
changes without supervision?

## What

A single executable that accepts a repository path and a task description,
drives an OpenAI-compatible model through a tool-calling loop backed by a
harvested search engine, commits the result to a branch, and exits with a
machine-readable report of what changed and what it cost.

One process, one job, one repository. Parallelism is N processes.

## Constraints

| Constraint | Value |
|---|---|
| Primary dev/verify platform | Windows 10 |
| Deploy target | Linux container |
| Distribution | Single self-contained binary |
| Provider | Any OpenAI-compatible chat completions endpoint |
| Model selection | Operator-curated allowlist plus runtime capability gating |
| Retrieval | Harvested engine, BM25 + AST only; no embedding model, no weights in the image |
| Concurrency | Blocking I/O, process-per-job. **No async runtime** |
| TLS | `rustls` only — `openssl-sys` must not enter the dependency tree |
| Git | Shell out to the `git` CLI — `git2`/`libgit2` must not be linked |
| Dependency policy | Exact version pins (`=x.y.z`) |
| Edition / toolchain | Edition 2024, `rust-version = "1.97"`, `unsafe_code = "deny"` |

## Must

- Vendor the supplied `engine/` source tree intact rather than reimplementing
  chunking, ranking or the dependency graph.
- Construct the index with `encoder: None`; feature-gate the encoder off by
  default so the embedding and array crates leave the build.
- Expose **at most seven** tools (six through M1; see "Amendment: the seventh
  tool"), with flat argument schemas — strings and integers only, no nested
  objects, no arrays of objects.
- Enforce a two-layer model gate before the first completion call.
- Refuse any model whose catalogue entry does not advertise native tool-calling
  support. No flag bypasses this.
- Enforce a hard cumulative token budget, checked **before** each API call.
- Enforce a hard iteration ceiling.
- Emit exactly one JSON object on stdout at completion, nothing else.
- Emit an append-only JSONL event log, flushed after every write.
- Use distinct exit codes for completion, budget breach, iteration breach,
  gating rejection and provider failure.
- Preserve per-file line-ending convention and UTF-8 BOM across writes.
- Reject `write_file` content that appears to elide unchanged code.
- Kill the full process tree on `run_command` timeout.
- Confine all filesystem access to the repository root.
- Commit partial work on budget or iteration breach rather than discarding it.
- Carry the `ORIGIN.md` and licence file that accompany the supplied engine
  source.

## Must Not

- Introduce an async runtime, `openssl-sys`, `git2`, or the embedding/array
  crates.
- Reimplement search, chunking, outline or the dependency graph.
- Depend on any crate outside this repository other than the pinned third-party
  set. This binary is standalone.
- Implement planning, decomposition, sub-agents or orchestration patterns. An
  orchestrator, if wanted later, sits above this binary and drives it through
  the stdout and exit-code contract.
- Add a `finish` tool — loop termination is a message with no tool calls.
- Truncate, summarise or otherwise manage conversation context.
- Retry the API call when tool arguments fail to parse — return the error to the
  model instead.
- Estimate cost the provider did not report.
- Hardcode a model list into the binary.
- Log file contents or full tool results into the event log.
- Implement rate limiting inside the runner.

## Out of Scope

Human-in-the-loop approval inside the runner; job queue or scheduler; streaming;
MCP server mode; web UI or HTTP API; multi-repo section querying via git API;
context window management; SEARCH/REPLACE or diff-based edit formats;
embedding-based similarity search.

## Current State

Not greenfield. A self-contained AST search engine source tree is supplied for
vendoring — roughly 7,900 lines across 19 modules: tree-sitter across fifteen languages,
BM25, ranking with boosting/penalties/weighting, outline, dependency graph,
chunking, gitignore-aware file walking.

**Verified facts that make the harvest viable:**

- The `engine/` subtree has zero `use crate::` references outside itself.
- The only external-crate references in the surrounding source are three uses of
  an `ExitCode` type, all **outside** `engine/`. Nothing inside `engine/` needs
  them.
- The encoder is already `Option<StaticEncoder>` in the index modules. Passing
  `None` yields BM25 + AST with no code change.
- The supplied tree has already had its own CLI entry point stripped, so the
  module is shaped to be called by a binary.

**Verified gap:**

- The index type has **no persistence**. Construction is in-memory only. Chunk
  types derive `Serialize` but not `Deserialize`, and there is no save or load
  path. Mounting a prebuilt index would require a serde round-trip across the
  chunk set, the BM25 index and a ~1,650-line dependency graph. WP5 measures
  before that is built.

**Public surface already declared** by the supplied tree: a dependency graph
type, an index type, a chunk type, an index-stats type and a search-result type,
plus the `search`, `outline`, `plan` and `digest` modules. Use whatever names the
supplied source declares — vendored type names are not renamed, since renaming
would break the byte-identical requirement in AC7.

---

## Execution Protocol

M1 runs as five work packages. Each is one Claude Code session with Sonnet.
Sessions do not span packages — context degrades and the gate loses meaning.

### The stage sequence within each work package

1. **Kickoff** — read this spec's package section and the acceptance criteria.
   Restate the scope in your own words. If any criterion is ambiguous, stop and
   ask; do not resolve it yourself.
2. **Feasibility** — confirm the named files exist, the named symbols resolve,
   and nothing in Current State has drifted. Report anything that contradicts
   the spec. A contradiction ends the package.
3. **Plan** — produce the approach, the files to create or modify, the test
   strategy, and the risk flags. Write it to `PLAN.md` at the repo root.
4. **Plan gate — STOP.** Print the plan summary and halt. Do not proceed on a
   plan you wrote yourself. Resume only on explicit approval in the session.
5. **Implement** — write the code.
6. **Test** — write and run the tests named in the package's acceptance
   criteria.
7. **Verify** — run every acceptance criterion and report pass or fail per line.
8. **Package gate — STOP.** Report the criteria table. Do not start the next
   package.

### Rules that no step overrides

**Nothing is staged, committed or pushed.** Not at any stage. The developer
reads every generated file and stages it. An agent that commits has taken the
decision the review exists to make. This applies to the runner's own repository
during development; it says nothing about what the finished binary does to
target repositories, which is specified separately in WP4.

**A gate is a stop, not a checkpoint.** The run halts until a human answers in
the session. There is no flag, phrasing or inference that answers a gate.

**Report what ran, not what was intended.** A criterion that was not exercised
says `not run`. A blank reads as a pass, and a pass nobody measured is worse
than no result at all.

**Never carry a step forward on an assumption about the one before it.** If
feasibility flagged a discrepancy, the plan addresses it or the package stops.

### Reporting format

End every package with:

```
## WP<n>: <name>

| criterion | outcome |
| --- | --- |
| AC1 | pass |
| AC2 | fail — <one line why> |
| AC3 | not run — <one line why> |

stopped at: <stage>
next: <what a human must do>
```

---

## Work Packages

---

## WP1 — Harvest the engine

**Scope.** Stand up the project and vendor the engine in unmodified.

### T1.1: Project skeleton

Create the cargo project. `Cargo.toml` with edition 2024, `rust-version 1.97`,
`unsafe_code = "deny"`, release profile tuned for a distributed binary. Copy the
exact tree-sitter pins supplied with the engine source — the core crate pin plus
the fifteen grammar pins — as a set. Grammar crates pin incompatible core versions; that set is
known to co-resolve and substituting fresh resolutions reintroduces a solved
problem.

**Files:** `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `README.md`

### T1.2: Lift the engine

Copy the supplied `engine/` subtree verbatim into `src/engine/`. Write a minimal
`lib.rs` re-exporting the declared public surface, and an `error.rs` defining
this project's own error type — `codemason_core::Error` — with no reference to any
external crate's error or exit-code types. Carry `ORIGIN.md` beside the engine
and its licence file at the crate root.

Put the encoder module behind a cargo feature `embeddings`, default off.

**Files:** `src/lib.rs`, `src/error.rs`, `src/engine/**`, `ORIGIN.md`,
`LICENSE-ENGINE`

**Do not refactor the copied code.** Not naming, not formatting, not error
handling, not "obvious" simplifications. It is adopted source with behaviour
nobody in this project has re-derived. Every edit made here is an edit that has
to be re-verified against tests that do not exist. If something looks wrong,
note it in the report and leave it.

### T1.3: Index wrapper

Thin wrapper over the index constructor with `encoder: None`. Expose build,
`search`, `chunks`, `graph`, `stats`. Time the build and record it in the stats
struct. The embedding-based similarity call is unavailable without the feature —
return a clear error naming the missing feature, not a panic.

**Files:** `src/index.rs`

### Acceptance criteria

- **AC1** `cargo build --release` succeeds on Windows 10.
- **AC2** `cargo tree` shows no embedding crate, no array crate, no `openssl`,
  no `git2`, no async runtime, and no path or git dependency outside this
  repository.
- **AC3** `cargo build --release --features embeddings` also succeeds.
- **AC4** Against a real C# repository, the index builds and reports a plausible
  chunk count.
- **AC5** A search for a known symbol returns its defining chunk in the top
  results.
- **AC6** The similarity call returns an error naming the missing feature.
- **AC7** A recursive diff of `src/engine/` against the supplied source tree is
  empty.

**Gate.** AC7 is the one that matters. If the copied tree differs, say where and
why before anything else proceeds.

---

## WP2 — CLI, configuration, model gating

**Scope.** Everything that must be correct before a single token is spent.

### T2.1: CLI surface

`clap` builder API. Three subcommands:

`codemason run --repo <PATH> --task <TEXT|@FILE>` with `--model`,
`--models-config`, `--base-url`, `--api-key` (env fallbacks), `--budget-tokens`
(default 200000), `--budget-usd`, `--max-iterations` (default 40), `--branch`,
`--log`, `--dry-run`, `--allow-unlisted-model`, `--verbose`.

`codemason models [--check]`.

`codemason index --repo <PATH> [--stats]` — builds and reports, no model involved.

Exit codes: `0` completed, `1` unrecoverable error, `2` budget exceeded, `3` max
iterations exceeded, `4` model rejected by gating, `5` provider error after
retries. These are a contract — a future orchestrator dispatches on them.

**Files:** `src/bin/codemason.rs`, `src/cli.rs`

### T2.2: Allowlist

Parse `models.toml`: ordered `[[model]]` tables with `id` and `role`, plus
`[gating]` with `min_context_length`, `require_tool_support`, `allow_unlisted`.
Resolution order: `--models-config`, `./models.toml`, then the platform config
directory. First entry is the default. Ship a sample with placeholder ids and a
comment stating that real ids must be validated with `codemason models --check`
before use.

**Files:** `src/config.rs`, `models.toml`

### T2.3: Capability gate

Fetch the provider's model catalogue. Reject with exit 4 if any hold:

1. The id is absent from the catalogue.
2. `supported_parameters` does not contain `tools`.
3. `context_length` is below the configured minimum.
4. The id denotes a router or auto-select pseudo-model. Non-deterministic
   selection makes cost attribution and failure diagnosis impossible.

Cache the catalogue with a 24-hour TTL so parallel processes do not each fetch
it. A fetch failure is non-fatal only with a valid cache entry.

`--allow-unlisted-model` skips check 1 only. **Check 2 has no bypass under any
flag.** A model that cannot call tools cannot complete a run, and letting it
start wastes money.

**Files:** `src/gating.rs`

### Acceptance criteria

- **AC1** `codemason --help` lists three subcommands; every documented flag appears.
- **AC2** `codemason index --repo . --stats` works end to end.
- **AC3** `codemason models` prints the allowlist in order; malformed TOML exits 1
  naming file and parse error; a file missing from all locations exits 1 listing
  the paths searched.
- **AC4** `codemason models --check` against a live catalogue exits 0 with all
  entries passing.
- **AC5** A model lacking `tools` exits 4 with no completion call made.
- **AC6** An absent id exits 4.
- **AC7** An auto-router id exits 4.
- **AC8** An unlisted id exits 4 without the flag and proceeds with it.
- **AC9** A second invocation within the TTL makes no network call.

**Gate.** AC5 is the criterion this package exists for. Demonstrate that no
completion request was issued, not merely that the exit code was 4.

---

## WP3 — Client, tools, loop

**Scope.** The read-only agent. No writes, no commands, no git.

### T3.1: LLM client

Blocking HTTP. Chat completions with `model`, `messages`, `tools`,
`tool_choice: "auto"`, and a usage-inclusion flag. Parse the assistant message
and usage. Accumulate prompt, completion and total tokens plus cost, keyed by
model id.

Retry 429 and 5xx with exponential backoff and jitter: base 1 s, max 5 attempts,
cap 60 s. Exhaustion maps to exit 5. Absent or malformed usage: record zeros,
flag it, continue. Never estimate cost the provider did not report.

**Files:** `src/llm/mod.rs`, `src/llm/types.rs`

### T3.2: Path safety and text handling

- Canonicalise relative paths against the repo root; reject anything resolving
  outside it as an error value, never a panic.
- Normalise to forward slashes in all tool inputs and outputs; convert to
  platform paths only at the filesystem boundary.
- On read: detect dominant line ending, strip UTF-8 BOM, present as LF. Record
  the per-file convention for the run.
- On write: restore the recorded convention and BOM.
- Elision detection: true when new content is under 50% of existing length
  **and** contains a line matching `... rest of`, `unchanged`, or
  `... existing`, case-insensitive.
- Enable Windows long-path support; verbatim prefixes where needed.
- Set stdout to UTF-8 explicitly.

**Files:** `src/text.rs`

### T3.3: Tool surface

Six tools, flat schemas, string and integer arguments only.

| Tool | Arguments | Notes |
|---|---|---|
| `context_search` | `query`, `max_results` | Engine-backed. **Primary discovery tool.** Returns `path:start-end`, chunk identifier, preview |
| `context_outline` | `path` | Symbol outline for one file |
| `read_file` | `path`, `start_line`, `end_line` | 1-based inclusive; 0 means unbounded. Line-numbered. Cap 2000 lines. Refuse binary (null byte in first 8 KB) |
| `write_file` | `path`, `content` | WP4 |
| `list_files` | `path`, `pattern`, `max_results` | Engine's gitignore-aware walker. Always excludes `.git/` and `.agent/` |
| `run_command` | `command`, `timeout_seconds` | WP4 |

Deliberately **not** separate tools: dependency and impact queries. Fold
dependency-graph information into `context_search` results as related paths.
Every additional tool costs accuracy on weaker models — the constraint is not
aesthetic.

The registry must be the only place aware of the tool list.

**Files:** `src/tools/mod.rs`, `src/tools/context.rs`, `src/tools/fs.rs`

### T3.4: The loop

Seed history with system prompt plus task. Call the model. Append the full
assistant message including content alongside tool calls. Execute tool calls in
order. Append one `tool` message per call. Repeat. Terminate on an assistant
message with no tool calls — that message is the summary.

In-loop error handling, none of which retries the API call:

- Tool arguments not valid JSON or missing required fields: append a `tool`
  message describing precisely what was wrong. Abort exit 5 after three
  consecutive parse failures of the same tool.
- Unknown tool name: append a `tool` message listing valid names.
- Three consecutive responses lacking usage data: abort exit 5.

Expose the loop as a library function taking config and returning a result
struct. `main` is a thin caller.

System prompt, deliberately short: the agent operates on a git repository at the
given path; discovery starts with `context_search`, then `context_outline`, then
`read_file`; when complete, reply with a summary and no tool calls.

**Files:** `src/loop.rs`

### T3.5: Event log

Append-only JSONL. Envelope: `ts` (RFC3339 with milliseconds), `run_id`
(UUID v7), `seq`, `type`.

Types: `run_started`, `index_built`, `model_gated`, `model_unlisted`,
`llm_call`, `tool_call`, `tool_result`, `usage_missing`, `budget_exceeded`,
`max_iterations_exceeded`, `run_completed`, `run_failed`.

Flush after every write. Record sizes and truncation flags, never file contents
or full tool results — the log is for cost accounting and diagnosis, and
payloads make it useless for both.

**Files:** `src/log.rs`

### Acceptance criteria

- **AC1** A debug round trip returns a parsed message and non-zero token counts.
- **AC2** A stub returning 429, 429, 200 succeeds; 500 five times exits 5; a
  response with usage removed does not panic.
- **AC3** Unit tests: traversal paths rejected; CRLF round-trips CRLF; BOM
  round-trips with BOM; LF gains no CRLF; elision fires on 40%-length content
  with a marker and not without one; a path over 260 characters is readable.
- **AC4** `context_search` for a known symbol returns its defining chunk first.
- **AC5** `context_outline` lists the expected members of a known class.
- **AC6** Every tool's JSON schema contains only string and integer properties
  at depth one.
- **AC7** "Describe the structure of this repository" completes, uses
  `context_search` before `read_file`, and terminates with a summary.
- **AC8** A fabricated malformed tool-call response continues the loop without
  panicking; a fabricated unknown tool name likewise.
- **AC9** A completed run's log parses line by line with contiguous `seq`, and
  contains `run_started`, `index_built`, at least one `llm_call`, and
  `run_completed`. Killing mid-run leaves every complete line parseable.

**Gate.** AC7 is the checkpoint for the whole project — the first point at which
the premise is tested. If the model does not behave sensibly here, stop and
report rather than proceeding to WP4.

---

## WP4 — Writes, commands, git, budget

**Scope.** Everything that changes state.

### T4.1: write_file

Whole-file replacement. Creates parent directories and the file if absent.
Applies WP3 line-ending and BOM restoration.

Rejections, each returning an error result the model can act on rather than
failing the run: content over 500 KB; path outside the repo root; elision
detected, with an error instructing the model to supply the complete file;
`--dry-run` set, with an error stating the write was simulated.

Extend the system prompt: `write_file` requires complete file content, never
fragments or markers such as `// ... rest unchanged`.

Whole-file replacement is deliberately the crudest option. It is
token-expensive and unambiguous. That trade is what makes M1 shippable and must
not be optimised away inside it.

### T4.2: run_command

Executes with cwd at the repo root, via the platform shell. Default timeout
120 s, maximum 900 s. Kill the entire process tree on timeout — a build that
outlives its parent holds file locks and breaks the next iteration. Capture
stdout and stderr interleaved, capped at 100 KB; when truncated keep the **last**
100 KB, since build errors appear at the end. Always return the exit code — a
non-zero exit is a normal result, not a tool failure. Refuse under `--dry-run`.

No command allowlist. Document in the README that the isolation boundary is the
container and the disposable repository copy.

Extend the system prompt: verify work by running the project's build or test
command before finishing.

### T4.3: Git

All repository operations route through one module so worktree isolation can be
substituted later without touching the loop.

Preflight: confirm the path is a git working tree; confirm clean, exit 1 with a
clear message if not, unless `--dry-run`. Create and checkout the branch.

Postflight: stage all, commit only if something changed, capture the commit SHA
and changed paths.

This is the finished binary committing to a **target** repository on its own
branch. It does not contradict the development-time rule that Claude Code stages
nothing in the runner's own repo.

### T4.4: Budget, ceiling, report

Check the budget immediately **before** each API call, never after — checking
after means the breaching call has already been paid for. Refuse to start when
the budget is at or below zero. Enforce a cost ceiling identically when the
provider reports cost.

On breach: log, break, commit, exit 2. On iteration ceiling: log, break, commit,
exit 3. Neither is an error state; partial work is often useful.

Emit exactly one JSON object on stdout, nothing else; diagnostics to stderr:
`run_id`, `status`, `exit_code`, `branch`, `commit`, `files_changed`,
`iterations`, `index` (chunk count, build ms), `models_used` (array — one entry
in M1, array so a later milestone can attribute planning and execution to
different models), `totals`, `duration_ms`, `log_path`.

**Files:** `src/tools/fs.rs`, `src/tools/exec.rs`, `src/repo.rs`,
`src/loop.rs`, `src/bin/codemason.rs`, `README.md`

### Acceptance criteria

- **AC1** Write to a CRLF fixture leaves it CRLF with a minimal git diff; a BOM
  fixture retains its BOM.
- **AC2** Content with an elision marker at under 50% length is rejected and the
  file is unmodified. A 600 KB write is rejected.
- **AC3** `--dry-run` leaves the filesystem untouched while the loop completes.
- **AC4** A command producing 200 KB returns the final 100 KB with a truncation
  notice; a command exiting 1 returns 1 as a normal result.
- **AC5** A command spawning a child that outlives its shell is fully terminated
  on timeout with no lingering file locks.
- **AC6** A successful run leaves a branch with exactly one commit containing
  the intended change; a run changing nothing produces no commit.
- **AC7** A dirty worktree without `--dry-run` exits 1 before any API call.
- **AC8** `--budget-tokens 1` exits 2 with zero API calls. A budget breached
  mid-run exits 2 with partial work committed and totals matching the sum of
  `llm_call` events. `--max-iterations 2` on a larger task exits 3 with partial
  work committed.
- **AC9** `codemason run ... > out.json 2> err.log` yields a file parsing as one
  JSON object; `--verbose` adds nothing to stdout; every exit path emits a
  matching `exit_code`.

---

## WP5 — Measure, containerise, accept

### T5.1: Index cost measurement — decision task

Run `codemason index --stats` against three real repositories of different sizes,
including the largest available. Record chunk count and build duration, cold and
warm OS cache.

This is a decision, not a build. The outcome determines whether index
persistence is worth building:

- **Under ~5 s on the largest** — build in-memory per run, do nothing further.
- **Over ~15 s** — persistence earns its place. It becomes M2: add
  `Deserialize` alongside the existing derives, a versioned on-disk format keyed
  by commit SHA, `codemason index --out`, and `codemason run --index` loading read-only.
  Treat a version or SHA mismatch as a rebuild, never a silent stale read.
- **Between** — defer. Revisit when parallel job counts make it visible.

Record the numbers in the README either way, so the decision is reviewable.

### T5.2: Container

Multi-stage: slim Rust builder, slim Debian runtime with `git` and CA
certificates, apt lists removed. `git` is required at runtime because the runner
shells out rather than linking a git library. No model weights — that is the
point of the BM25-only choice.

### T5.3: Acceptance suite

Integration tests against a fixture repository plus a stub provider server. The
fixture must be a small C# project containing at least one CRLF file and one
UTF-8 BOM file.

Consolidate every AC from WP1 through WP4 as automated tests where automatable,
plus:

- Two concurrent runs against two repository clones both succeed with
  independent logs and branches.
- A full successful run: exit 0, branch, commit, valid stdout JSON.

The concurrency case validates the premise of the whole project and must not be
deferred.

### Acceptance criteria

- **AC1** Three index measurements recorded with repo size, chunk count and
  duration; a stated decision with reasoning in the README.
- **AC2** Container builds; size recorded; a run inside it against a mounted
  repository completes with no model weights present.
- **AC3** `cargo test --release` passes the full suite on Windows 10.
- **AC4** Two concurrent runs against two clones succeed independently.

---

## Amendment: the seventh tool

**Status:** accepted after M1's five packages reached their gates. Recorded
here rather than applied silently, because "at most six tools" was stated
three times in this document as load-bearing and a number that moves without
a written reason is a number that keeps moving.

`web_search` is added, raising the cap from six to seven.

**What it does.** `web_search(query, max_results)` — flat schema, string and
integer, consistent with every other tool. Provider-agnostic for the same
reason the model client is: an endpoint and key are read from
`CODEMASON_SEARCH_URL` and `CODEMASON_SEARCH_API_KEY`, and the response is
read structurally rather than against one vendor's schema. No new crate
enters the tree — `ureq` and `regex` are already pinned dependencies. With no
provider configured it falls back to a keyless DuckDuckGo endpoint that is
explicitly **best-effort**: that endpoint rate-limits and answers HTTP 202
with a challenge page often enough that nothing should depend on it.

**Why it earns the slot.** The engine answers "what is in this repository".
Nothing answered "what does this library's API look like" or "what does this
compiler error mean", and a model that cannot look those up guesses instead —
which costs iterations against exactly the budget this binary exists to
protect.

**What it costs, stated plainly.** The original reasoning still holds: every
additional tool costs accuracy on weaker models, and tool count is where
cheap models degrade first. Seven is a budget, not a formality. The tool
description steers explicitly away from `context_search`'s territory
("never use it to look for code that is in the repository") because
overlapping discovery tools are precisely the confusion the cap exists to
prevent. If measurement shows weaker models reaching for `web_search` when
they should be searching the repository, the correct response is to remove it
again, not to add an eighth tool to compensate.

## Milestone Validation

M1 is complete when:

1. All five package gates have been answered and every AC is `pass`.
2. Ten consecutive real runs against varied tasks on at least two repositories
   succeed with exit 0 and no manual intervention.
3. **At least three of those runs used a different model vendor.** Vendor
   independence is the point of this binary; if it only works on one vendor's
   models, M1 has not succeeded.
4. At least one run used `run_command` to verify its own work.
5. Four concurrent processes against four clones complete with correct
   independent cost attribution.
6. Reported token totals reconcile exactly with the sum of `llm_call` events.
7. `cargo tree` shows no async runtime, `openssl`, `git2`, embedding or array
   crates, or any dependency outside this repository.
8. Index build cost measured and the persistence decision recorded.

If validation 2 or 3 fails repeatedly on a given model, that is a **finding, not
a bug** — record which models pass and tighten the allowlist. Producing that
list is one of M1's genuine outputs and the input to every cost decision after
it.

---

## Task Design Principles

**One work package per session.** Do not carry a session across a gate. Context
degrades, and a gate answered in a session that has already drifted is not the
approval it appears to be.

**WP1/AC7 and WP3/AC7 are the two checkpoints.** The first proves the engine
lifts cleanly; if it does not, everything downstream changes. The second is the
first test of the premise. Reaching both quickly matters more than polishing
what sits between them.

**Harvest, do not improve.** Copy the engine intact. Resist refactoring it to
taste; every change made there is a change nobody has re-verified.

**Fewer tools, flatter schemas.** Every temptation to add a tool or nest an
argument runs against the reason this binary exists.

**Errors the model can act on are not failures.** Malformed arguments, unknown
tools, rejected writes, non-zero command exits all return a descriptive result
into the conversation and the loop continues. Only exhausted retries, repeated
consecutive failures and provider errors terminate a run.

**Do not weaken the gate to make a model work.** If a model fails WP2, remove it
from the allowlist rather than relaxing the check.

**Nothing in this binary knows about orchestration.** The stdout contract and
the exit codes are the whole interface.
