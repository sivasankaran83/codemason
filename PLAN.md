# PLAN.md — WP3: Client, tools, loop

## Scope (restated)

The read-only agent. No writes, no commands, no git — those are WP4. Five
tasks:

- **T3.1** Blocking LLM client: OpenAI-compatible chat completions, retry on
  429/5xx with backoff+jitter, usage/cost accumulation keyed by model id,
  never estimate cost the provider didn't report.
- **T3.2** Path safety (traversal rejection, Windows long paths) and text
  handling (line-ending/BOM detect-and-restore, elision detection, stdout
  UTF-8).
- **T3.3** Six tools, flat string/int schemas, one registry as the sole
  source of truth for the tool list.
- **T3.4** The tool-calling loop: seed system+task, call model, execute tool
  calls in order, append one `tool` message per call, repeat; terminate on an
  assistant message with no tool calls. In-loop error handling for malformed
  args / unknown tool / missing usage — none of it retries the API call.
- **T3.5** Append-only JSONL event log, flushed after every write, fixed
  envelope and type set, no payload contents.

Gate: AC7 ("describe the structure of this repository" completes, uses
`context_search` before `read_file`, terminates with a summary) is called out
in SPEC.md as the first test of the whole project's premise.

## Feasibility findings

None of WP3's target files exist yet (`src/llm/`, `src/text.rs`,
`src/tools/`, `src/loop.rs`, `src/log.rs`) — expected, this package creates
them.

Symbols WP3 depends on from WP1/WP2 all resolve as documented, confirmed by
direct read rather than assumed:

- `codemason_core::index::Index::{build, search, chunks, graph, stats}` —
  `search(&self, query: &str, top_k: usize) -> Vec<SearchResult>` (WP1's
  wrapper hardcodes `alpha`/`filter_languages`/`filter_paths` to `None`,
  matching `context_search`'s flat `query`/`max_results` schema with no
  extra knobs to expose).
- `engine::graph::DependencyGraph::{deps, dependents}` —
  `deps(&self, file_path: &str) -> Option<&FileNode>` where `FileNode` carries
  `symbols: Vec<Symbol{name, kind, line}>` and `depends_on: Vec<String>`;
  `dependents(&self, file_path: &str) -> Vec<&str>`. This is what
  `context_outline` reads for a file's symbol list, and what
  `context_search` folds in as related paths (SPEC.md: "Fold
  dependency-graph information into `context_search` results as related
  paths" instead of separate dependency/impact tools).
- `engine::types::{Chunk, SearchResult, MatchLine}`, `Chunk::location()` →
  `"path:start-end"`.
- `engine::file_walker::{walk_files, default_ignored_dirs}` and the `ignore`
  crate's `WalkBuilder`/`OverrideBuilder` (already a pinned dependency, used
  the same way inside `engine::file_walker`) — `list_files` reuses this
  pattern directly rather than the engine's extension-filtered
  `walk_files` (list_files must list *any* file, not just recognized source
  extensions).
- `cli::{RunArgs, ExitCode, resolve_credential}`, `config::ModelsConfig`,
  `gating::{catalogue, check}`, `error::Error` — `bin/codemason.rs`'s
  `run_cmd` currently stops right after gating passes
  (`"execution loop is not implemented until WP3"`); WP3 replaces that stub
  with the real build-index → construct-client → run-loop → emit-log
  sequence.
- `tests/common::StubServer` exists but only serves one fixed 200 response to
  every request regardless of path — insufficient for WP3's retry tests
  (429/429/200, 500×5) and for driving a full `run` invocation through both
  `GET /models` (gating) and `POST /chat/completions` (the loop). Extending
  it, not replacing it — `tests/gating.rs`'s existing use of
  `StubServer::start` must keep compiling unchanged.

New dependencies needed, neither in the current dependency tree as a direct
dep:

- `uuid = "=1.25.0"`, feature `v7`, for the event log's `run_id`. Not present
  even transitively — `cargo add uuid --features v7 --dry-run` resolves
  1.25.0 against the existing lockfile.
- `rand = "=0.9.5"` — **already** in the tree transitively (`model2vec-rs` →
  `hf-hub`/`tokenizers` → `rand`, confirmed via `cargo tree -i rand`; note
  `model2vec-rs` is always-compiled per WP1's `Cargo.toml` comment, so this
  is not a new resolution, only a promotion to direct dependency, same move
  WP2 made for `clap`/`ureq`). Used for retry-jitter, avoiding a hand-rolled
  entropy source.

`cargo tree` re-check after the two additions: still no `openssl`, `git2`,
`tokio`/`async-std`, or embedding/array crates beyond the already-accepted
`model2vec-rs`/`ndarray` pair from WP1. No contradiction with SPEC.md's
Current State section. The package proceeds.

## Ambiguity resolved at kickoff

**Tool registry scope in WP3 vs WP4.** T3.3's table lists all six tools,
`write_file` and `run_command` annotated "WP4", but WP3's own scope line says
"no writes, no commands" and T3.3's file list has no `exec.rs`. Confirmed:
**register all six schemas now**; `write_file`/`run_command` handlers return
a descriptive "not available until WP4" tool-result error rather than acting
— the model can read that and continue, per the errors-are-not-failures
design invariant. This satisfies AC6 ("every tool's JSON schema...") against
the full, final six-tool set from this package onward and matches the "at
most six, fixed up front" framing in CLAUDE.md, rather than growing the
registry's shape between packages. WP4 swaps the two stub handlers for real
`fs.rs`/`exec.rs` logic without touching the schemas.

## Other inferences (flagged, not asked — same treatment WP2 gave the
`/models` endpoint path)

1. **Chat completions endpoint & usage flag.** `POST {base_url}/chat/completions`,
   OpenAI-compatible, mirroring WP2's `{base_url}/models` convention.
   SPEC.md's "usage-inclusion flag" is read as OpenRouter's
   `"usage": {"include": true}` request field — the one OpenRouter-specific
   flag that both guarantees a `usage` block on a non-streaming response *and*
   adds a provider-reported `cost` field, which is what makes "accumulate...
   cost, keyed by model id" (T3.1) meaningful without ever estimating it
   ourselves. Sent unconditionally; unknown fields are ignored by
   OpenAI-compatible endpoints that don't recognize it.
2. **Retry/backoff formula.** `delay = min(60s, base=1s * 2^attempt) + jitter`,
   jitter drawn from `rand` (now a direct dep) as `0..=250ms`, attempts
   capped at 5. Only `429` and `5xx` HTTP statuses retry; anything else
   (4xx other than 429, connection failure) is a single-shot provider error.
3. **Elision markers.** Case-insensitive substring match for the three
   literal marker phrases from SPEC.md's own backticked text — `"... rest of"`,
   `"unchanged"`, `"... existing"` — combined with the `new.len() <
   existing.len() / 2` length check. `write_file` doesn't exist as working
   code until WP4, but the detection function itself is T3.2's, tested
   directly against strings.
4. **Windows long-path support.** `std::fs::canonicalize` already returns
   `\\?\`-prefixed verbatim paths on Windows, which is what actually lifts
   the 260-char `MAX_PATH` limit — there is no additional opt-in needed, and
   none is added. `text.rs`'s path-safety function canonicalizes before any
   filesystem call, so this falls out for free. Verified by AC3's own test
   (a path over 260 chars, read via this path).
5. **Stdout UTF-8, without `unsafe_code`.** `unsafe_code = "deny"` is a lint
   on *this crate's* code, not on dependencies, but pulling in a
   console/terminal crate just to call `SetConsoleOutputCP` for one line is
   more than this needs. Rust's `std::io::Stdout` on Windows already routes
   through `WriteConsoleW` (UTF-16, codepage-independent) whenever stdout is
   an actual console, and raw bytes whenever it's redirected to a file or
   pipe (the JSON-report case, where codepage is irrelevant since the reader
   decodes UTF-8 itself). "Set stdout to UTF-8 explicitly" is implemented as:
   always write pre-encoded UTF-8 bytes via `write_all`, never anything
   locale-dependent (`println!` with formatting that could go through a
   lossy codec) — no FFI, no `unsafe`.
6. **Default `--log` path when omitted.** `<repo>/.agent/log/run-<run_id>.jsonl`.
   Not named in SPEC.md; inferred from T3.3's "Always excludes `.git/` and
   `.agent/`" in `list_files`, which implies `.agent/` is this tool's own
   working area inside the target repo. Parent directories created as
   needed.
7. **`--verbose`.** No WP3 acceptance criterion exercises it (WP4's AC9 only
   asserts it adds nothing to *stdout*). Implemented as: echo each tool call
   and its result's size/truncation to stderr as it happens. Low-risk,
   reversible if this reads wrong at the gate.
8. **`max_iterations` is threaded into `LoopConfig` but not enforced in
   WP3.** T4.4 owns "enforce a hard iteration ceiling" and the exit-3
   mapping; T3.4 defines no ceiling behaviour. The loop runs until a
   no-tool-call message or one of T3.4's own abort conditions (three
   consecutive same-tool parse failures, three consecutive missing-usage
   responses) fires. No ad hoc cap is added in its place — that would be
   guessing at WP4's contract. Practical effect: WP3's own automated tests
   use finite fabricated stub traces, so nothing in `cargo test` can hang;
   AC7 against a live model is exercised manually (see Test strategy).

## Approach

### `Cargo.toml`

```toml
uuid = { version = "=1.25.0", features = ["v7"] }
rand = "=0.9.5"
```

### `src/llm/types.rs`

Wire types for the OpenAI-compatible chat completions shape actually used:
`ChatMessage { role: String, content: Option<String>, tool_calls:
Option<Vec<ToolCall>>, tool_call_id: Option<String>, name: Option<String> }`
(the last two populate outgoing `tool` role messages), `ToolCall { id,
r#type: "function", function: FunctionCall{name, arguments: String} }`,
`ToolDef { r#type: "function", function: FunctionSpec{name, description,
parameters: serde_json::Value} }`, `Usage { prompt_tokens, completion_tokens,
total_tokens, cost: Option<f64> }` (all fields tolerant of absence —
`#[serde(default)]` — since "absent or malformed usage" must not panic),
request/response envelope structs.

### `src/llm/mod.rs`

```rust
pub struct Client { base_url: String, api_key: String, http: ureq::Agent }
pub struct CompletionResult { pub message: ChatMessage, pub usage: Option<Usage> }
pub struct Totals { pub prompt: u64, pub completion: u64, pub total: u64, pub cost: f64 }

impl Client {
    pub fn new(base_url: String, api_key: String) -> Self;
    pub fn complete(&self, model: &str, messages: &[ChatMessage], tools: &[ToolDef])
        -> Result<CompletionResult, Error>;
}

pub struct UsageLedger { totals_by_model: HashMap<String, Totals> }
impl UsageLedger {
    pub fn record(&mut self, model: &str, usage: Option<&Usage>);
    pub fn totals(&self) -> &HashMap<String, Totals>;
}
```

`complete` builds the request body (`model`, `messages`, `tools`,
`tool_choice: "auto"`, `usage: {"include": true}`), POSTs with `ureq`,
retries `429`/`5xx` per the backoff formula above, and on the 5th exhausted
attempt returns `Error::ProviderExhausted { model, attempts, last_status }` —
a new `error.rs` variant the caller (the loop) maps straight to exit 5. A
non-retryable status (other 4xx, transport error) returns a distinct
`Error::ProviderRequest` variant on the first attempt, also mapped to exit 5
by the caller — SPEC.md's Must list only names *retries* for 429/5xx,
nothing about tolerating a hard 4xx.

Parsing tolerates an absent/malformed `usage` object: `Option<Usage>::None`
if missing or if the JSON shape doesn't parse, never a panic — the loop
layer is what turns three consecutive `None`s into an abort.

### `src/text.rs`

```rust
pub fn to_repo_relative(repo_root: &Path, raw: &str) -> Result<PathBuf, Error>; // canonicalize + traversal check
pub fn normalize_slashes(path: &str) -> String;

pub enum LineEnding { Lf, Crlf }
pub struct ReadPresentation { pub content: String, pub line_ending: LineEnding, pub had_bom: bool }
pub fn read_for_model(bytes: &[u8]) -> ReadPresentation;      // BOM strip, CRLF->LF, detect dominant ending
pub fn restore_for_write(content_lf: &str, ending: LineEnding, had_bom: bool) -> Vec<u8>;

pub fn looks_elided(existing: &str, new_content: &str) -> bool;

pub fn init_stdout_utf8();  // documents/enforces the write_all-only discipline; see inference 5
```

`to_repo_relative` canonicalizes `repo_root` once, joins the raw
(forward-slash-normalized) path, canonicalizes the result, and rejects with
an `Error::PathEscapesRepo` value (never a panic) if it doesn't start with
the canonicalized root — every tool in `tools/fs.rs` and `tools/context.rs`
routes paths through this before touching the filesystem.

### `src/tools/mod.rs`

```rust
pub struct ToolDef { pub name: &'static str, pub description: &'static str, pub schema: serde_json::Value }
pub fn registry() -> Vec<ToolDef>;               // the six, in the SPEC.md table's order
pub fn as_llm_tool_defs() -> Vec<llm::ToolDef>;   // registry() reshaped for the wire format

pub enum ToolOutcome { Ok(String), Error(String) }  // Error still becomes a normal `tool` message
pub fn dispatch(name: &str, args_json: &str, ctx: &ToolContext) -> DispatchResult;
pub enum DispatchResult { Ran(ToolOutcome), UnknownTool, BadArguments(String) }
```

Every schema: `{"type":"object","properties":{...only "type":"string" or
"type":"integer" values...},"required":[...]}` — no nested `object`/`array`
property ever appears, checked by AC6's test walking the `serde_json::Value`
one level deep. `write_file`/`run_command` are present in `registry()` with
real schemas (`path`+`content`; `command`+`timeout_seconds`) but `dispatch`
returns `ToolOutcome::Error("write_file is not available until WP4")` /
same for `run_command`, per the resolved ambiguity above.

### `src/tools/context.rs`

- `context_search(query, max_results)`: `index.search(query, top_k)` where
  `top_k = if max_results == 0 { 10 } else { max_results as usize }` (0 = "use
  a sensible default", consistent with `read_file`'s existing 0-means-unbounded
  convention for line numbers, not literally zero results). Formats each
  result as `path:start-end (score=..)` + a short preview (first ~3 content
  lines) + related paths folded in from `graph.deps(file_path).depends_on`
  and `graph.dependents(file_path)`, only when non-empty.
- `context_outline(path)`: `to_repo_relative` then `graph.deps(&normalized)`;
  `None` → `ToolOutcome::Error("no outline available for {path}")` (unsupported
  language or file not in the index — not a panic); `Some(node)` → one line
  per `Symbol{kind, name, line}`, plus `depends_on`.

### `src/tools/fs.rs`

- `read_file(path, start_line, end_line)`: `to_repo_relative`, read bytes,
  refuse if a null byte appears in the first 8 KB (`ToolOutcome::Error`,
  "binary file"), else `text::read_for_model`, slice `start_line..=end_line`
  (`0` = open end per SPEC.md), cap the slice at 2000 lines with a truncation
  note appended, number each line `"NNNN: ..."`.
- `list_files(path, pattern, max_results)`: `to_repo_relative` for `path`
  (root of the walk, `"."` default), `ignore::WalkBuilder` with
  `default_ignored_dirs()` plus `.git`/`.agent` always excluded, an
  `OverrideBuilder` glob from `pattern` when non-empty, capped at
  `max_results` (0 → default 100).
- Stub `write_file`/`run_command` handlers live in `tools/mod.rs`'s
  `dispatch`, not here — there is no real filesystem-mutating or
  process-spawning code to justify a WP3 `fs.rs` entry for them, and putting
  the stub message next to the schema keeps the "not available until WP4"
  behaviour in one obvious place.

### `src/loop.rs`

```rust
pub struct LoopConfig {
    pub repo_root: PathBuf,
    pub task: String,
    pub model: String,
    pub max_iterations: u32,   // threaded through, unenforced this package — inference 8
}
pub enum LoopExit {
    Completed { summary: String, iterations: u32 },
    ProviderError { reason: String, iterations: u32 },
}
pub fn run(
    cfg: &LoopConfig,
    client: &llm::Client,
    index: &Index,
    log: &mut log::EventLog,
) -> (LoopExit, llm::UsageLedger);
```

History seeded with the system message (repo path + the three-tool discovery
order + "reply with a summary and no tool calls when done", verbatim per
T3.4) and a user message holding `task`. Each turn: log `llm_call`, call
`client.complete`; on `Err` → log `run_failed`, return `ProviderError`
(caller maps to exit 5). On success: append the full assistant message
as-is (content *and* `tool_calls` together, per SPEC.md, not split apart).
No `tool_calls` → `Completed`. Otherwise, for each call in order: parse
`arguments` as JSON — failure appends a `tool` message describing exactly
what was wrong, increments a per-tool-name consecutive-failure counter, and
if that counter hits 3, returns `ProviderError`; success resets that
counter. Unknown tool name → `tool` message listing the six valid names, no
abort (SPEC.md only names three-consecutive-same-tool-parse-failures and
three-consecutive-missing-usage as abort conditions — an unknown name isn't
one of them). Missing/malformed usage on a response increments a separate
whole-run counter; 3 consecutive → `ProviderError`; any response with valid
usage resets it to 0.

### `src/log.rs`

```rust
pub struct EventLog { writer: BufWriter<File>, run_id: Uuid, seq: u64 }
impl EventLog {
    pub fn open(path: &Path, run_id: Uuid) -> Result<Self, Error>;
    pub fn write(&mut self, event_type: &str, fields: serde_json::Value); // envelope + flush, one line
}
```

One `write_all` of the fully-formed line (`{ts, run_id, seq, type, ...fields}`
serialized once) plus a line count under Windows terminates lines with a
single `flush()` right after — nothing is buffered across calls, satisfying
"flushed after every write" and keeping a mid-kill log's earlier lines
independently parseable. `seq` starts at 1 and increments per call,
contiguous by construction (single writer, no concurrent writers within one
process). Event field payloads follow SPEC.md's "sizes and truncation
flags, never file contents or full tool results" — e.g. `tool_result` logs
`{tool, ok, result_chars, truncated}`, never the string itself;
`run_started` logs `{repo, model, task_chars}`, never the task text.
`budget_exceeded`/`max_iterations_exceeded`/`model_gated`/`model_unlisted`
variant *names* exist in this file's event-type set now (SPEC.md lists all
of them under T3.5 as WP3's job to define), but nothing in WP3 emits the
budget/iteration ones — WP4 wires those call sites when the enforcement
they describe actually exists.

### `src/bin/codemason.rs`

`run_cmd`, after `gating::check` passes: resolve `--log` (or the
`.agent/log/run-<uuid>.jsonl` default), open the `EventLog`, log
`run_started`; `Index::build(&args.repo)` (mapping build failure to
`ExitCode::UnrecoverableError` same as `index_cmd` already does), log
`index_built`; construct `llm::Client`; call `loop_::run`; on `Completed`,
print the summary to stdout, log `run_completed`, exit 0; on
`ProviderError`, log already written by `loop_::run`, exit 5.

### `src/error.rs` additions

`ProviderExhausted { model, attempts, last_status }`,
`ProviderRequest { model, source }`, `PathEscapesRepo { path }`,
`LogOpen { path, source }` — no reference to any external crate's own error
or exit-code type, carried forward from T1.2/T2's rule.

## Test strategy, by acceptance criterion

- **AC1** `tests/llm_client.rs`: extended stub answers one `POST
  /chat/completions` with a canned assistant message + usage block;
  `Client::complete` called directly (library-level, no CLI spawn); assert
  the parsed message and `usage.total_tokens > 0`.
- **AC2** Same file: (a) stub sequence `[429, 429, 200]` on
  `/chat/completions` → `complete` succeeds, asserted attempt count == 3; (b)
  `[500, 500, 500, 500, 500]` → `Err(ProviderExhausted)`; (c) a `200`
  response with the `usage` key stripped from the JSON body → `complete`
  still returns `Ok` with `usage: None`, no panic.
- **AC3** `src/text.rs` unit tests, `#[cfg(test)] mod tests`, same convention
  as `config.rs`/`gating.rs`: traversal (`../../etc`) rejected as `Err`, not
  panic; a CRLF fixture round-trips CRLF; a BOM fixture round-trips with BOM;
  an LF fixture's write path never introduces `\r\n`; `looks_elided` true at
  40% length with each of the three markers present, false without any
  marker; a synthetic path exceeding 260 chars (nested temp dirs) reads
  successfully through `to_repo_relative`.
- **AC4/AC5** `src/tools/context.rs` unit tests against the real C# fixture
  under `old_source/` (same skip-if-absent pattern as `index.rs`'s AC4/AC5
  tests) — `context_search` for a known symbol returns it as the first
  result; `context_outline` on a known file lists its expected member
  symbols.
- **AC6** `src/tools/mod.rs` unit test: for each of the six `ToolDef`s, walk
  `schema["properties"]` one level and assert every value's `"type"` is
  `"string"` or `"integer"`, and that no property value itself contains a
  nested `"properties"` or `"items"` key.
- **AC7** Two parts, per the resolved-ambiguity precedent WP2 set for AC4:
  (a) `tests/loop_.rs`, automated — a fabricated multi-turn stub trace
  (assistant calls `context_search`, then `read_file`, then answers with no
  tool calls) drives `loop_::run` directly and asserts the call order and
  that it terminates `Completed`; this proves the loop's *mechanics*, not a
  live model's judgment. (b) A real `codemason run --repo . --task "Describe
  the structure of this repository"` against a live provider, run by hand
  once credentials are available, reported directly in the AC table — same
  treatment as WP2's AC4.
- **AC8** `tests/loop_.rs`: a stub trace with one malformed-JSON tool-call
  response and one unknown-tool-name response, each followed by a normal
  terminating response — assert the run completes normally (no panic, no
  premature abort, since two isolated bad calls don't reach the
  three-consecutive threshold).
- **AC9** `tests/event_log.rs`: (a) a full completed run's log file, parsed
  line by line, has contiguous `seq` and contains at least one each of
  `run_started`, `index_built`, `llm_call`, `run_completed`; (b) spawn
  `codemason run` against a stub that stalls for several iterations, kill
  the child mid-run (`Child::kill()`), then parse the log — every line
  except possibly a truncated final one must parse as valid JSON.

## Risk flags

1. **Endpoint path and `usage.include` flag are inferences** (same category
   as WP2's `/models` path) — flagged for sign-off.
2. **Retry/backoff exact formula** (base 1s, ×2 each attempt, 60s cap, 5
   attempts, 0–250ms jitter) is my own construction of "exponential backoff
   and jitter" — SPEC.md gives the four numeric bounds but not the formula
   shape.
3. **Elision marker strings** are read literally from SPEC.md's own
   backticked examples; a marker set that differs from what a real model
   actually writes (e.g. "rest remains the same") would still slip through
   undetected until WP4 exercises this against real model output.
4. **`--log` default path under `.agent/`** is inferred, not spec'd.
5. **AC7 is only partially automated** — the live-model half needs
   credentials this sandbox doesn't have; flagged the same way WP2 flagged
   its AC4.
6. **`max_iterations` unenforced this package** (inference 8 above) — if
   this reading is wrong and WP3 was expected to cap iterations itself, that
   changes `loop.rs`'s public shape before WP4 extends it.
7. **`StubServer` extension changes shared test infrastructure**
   (`tests/common/mod.rs`) that `tests/gating.rs` already depends on — the
   plan is additive (new methods, `StubServer::start` untouched) specifically
   to avoid destabilizing WP2's passing tests, but it's still a shared-file
   edit worth flagging.
