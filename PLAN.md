# PLAN.md — WP2: CLI, configuration, model gating

## Scope (restated)

Everything that must be correct before a single token is spent. Three tasks:

- **T2.1** CLI surface, `clap` **builder API** (not `derive`) — three
  subcommands: `run` (with `--repo`, `--task`, `--model`, `--models-config`,
  `--base-url`, `--api-key`, `--budget-tokens`, `--budget-usd`,
  `--max-iterations`, `--branch`, `--log`, `--dry-run`,
  `--allow-unlisted-model`, `--verbose`), `models [--check]`, and
  `index --repo <PATH> [--stats]`. Six exit codes (0/1/2/3/4/5) as a stable
  contract.
- **T2.2** Allowlist: parse `models.toml` (`[[model]]` tables with `id` +
  `role`, plus `[gating]`), resolve it from `--models-config` → `./models.toml`
  → platform config dir, first entry is the default model.
- **T2.3** Capability gate: fetch the provider's live model catalogue, reject
  (exit 4) an id that's absent from it, lacks `tools` in
  `supported_parameters`, is under the configured minimum context length, or
  is a router/auto-select pseudo-model. Cache the catalogue fetch 24h so
  concurrent processes don't each hit the network. `--allow-unlisted-model`
  has a narrow, specific bypass scope (below).

WP3 (the LLM client and the tool-calling loop) does not exist yet. `run` in
this package parses and validates everything up to the point a completion
call would be made, then stops — it does not build the index, does not call
the model, and `--dry-run`/`--budget-*`/`--branch`/`--log` are accepted and
stored but have no effect until WP3/WP4 exist to act on them.

## Feasibility findings

Named files from T2.1–T2.3 (`src/cli.rs`, `src/config.rs`, `models.toml`,
`src/gating.rs`) don't exist yet — expected, this package creates them.
`src/bin/codemason.rs` exists as WP1's placeholder `fn main() {}`.

Symbols WP2 depends on from WP1 all resolve as documented:
`codemason_core::Index::{build, search, chunks, graph, stats}` and
`BuildStats{indexed_files, total_chunks, languages, build_ms}` are `pub` in
`src/index.rs`, backed by `engine::SembleIndex::{from_path, search, stats,
chunks, graph}` in `src/engine/index/mod.rs` (confirmed by direct grep, not
assumed). `codemason index --repo . --stats` (AC2) needs nothing new from the
engine.

Dependency-tree check for the new crates this package needs: `Cargo.lock`
already carries `clap` (4.6.6) and `ureq` (2.12.1) as *transitive* deps of
`model2vec-rs` (WP1's always-compiled embeddings backend, per WP1's Cargo.toml
comment), and `rustls` is present with no `native-tls`, `openssl-sys`,
`git2`, `tokio`, or `async-std` anywhere in the tree. That means promoting
`clap` and `ureq` to direct dependencies at those same pinned versions won't
introduce a new major version or drag in anything the project's constraints
forbid. `toml` (for parsing `models.toml`) isn't in the tree at all yet — new
addition, exact version to be pinned to whatever `cargo add toml` resolves at
implementation time rather than guessed here.

No contradiction with SPEC.md's Current State section. The package proceeds.

## Ambiguities resolved at kickoff

Two things in WP2's acceptance criteria aren't fully determined by SPEC.md's
prose and materially change `gating.rs`'s behavior, so I stopped and asked
rather than picking silently:

**1. Router/auto-select detection (AC7).** SPEC.md never defines how to
recognize one. Resolution: ship a small built-in default list (exact id
`openrouter/auto`; id ends with `/auto`; id contains `auto-router`), and let
`[gating]` extend it with a `deny_id_patterns` array of substrings — a config
field not literally named in T2.2's list (`min_context_length`,
`require_tool_support`, `allow_unlisted`), added because the built-in list
alone is provider-specific and brittle. Flagging the field addition itself
for sign-off at this gate, separately from the mechanism it implements.

**2. What `--allow-unlisted-model` actually bypasses (AC6 vs AC8).** T2.3's
numbered check 1 ("the id is absent from the catalogue") and the sentence
"`--allow-unlisted-model` skips check 1 only" read, taken completely
literally, as if the flag lets an id through even when the live provider
catalogue doesn't serve it at all — which would leave checks 2–4
(tools/context/router) with no data to evaluate. That reading also makes
AC6 ("an absent id exits 4") and AC8 ("an unlisted id exits 4 without the
flag, proceeds with it") test the identical scenario under two names, which
is an unlikely thing for two separate ACs to do. Resolution, confirmed:
these are two distinct checks, only one of which SPEC.md numbers explicitly.

- **Allowlist membership** — is `--model <id>` present in the resolved
  `models.toml`? This is the *unnumbered*, earlier check. Skipped by
  `--allow-unlisted-model` (this is what "`allow_unlisted`" as a config field
  name, and "`--allow-unlisted-model`" as a flag name, both actually refer
  to). AC8's scenario.
- **Catalogue presence** — is the id in the *provider's live `/models`
  response* at all? This is T2.3's numbered check 1. Always enforced,
  no flag bypasses it — an id the provider doesn't serve has no data for
  checks 2–4 to run against regardless of operator intent. AC6's scenario.

So the full sequence for a given `--model <id>` is: allowlist membership
(skippable) → fetch/cache catalogue → catalogue presence (check 1, not
skippable) → tools (check 2, not skippable, ever) → context length
(check 3) → router pattern (check 4).

**3. AC4 needs a live provider.** `models --check` "against a live
catalogue" requires a real base URL and API key; this sandbox has neither
configured. You said you'll provide credentials before Verify. AC5–AC9 don't
need this — they're driven by a local stub HTTP server (no new dependency;
hand-rolled on `std::net::TcpListener`, mirroring what WP3's AC2 will need
for the real LLM client) that returns fabricated catalogue responses and
records whether a completions-shaped request ever arrives. AC4 itself will
be run once by hand against whatever you provide and reported directly in
the AC table, not wired into `cargo test` (a test that silently needs a
secret to pass is worse than an explicit manual step).

## Approach

### New dependencies (`Cargo.toml`)

```toml
clap = "=4.6.6"                                            # builder API — Command/Arg, not #[derive(Parser)]
toml = "=<resolved>"                                       # models.toml parsing — pin whatever `cargo add` resolves
ureq = { version = "=2.12.1", default-features = false, features = ["json", "tls"] }
```

`ureq`'s `tls` feature is its rustls backend (not `native-tls`) —
`default-features = false` makes that an explicit choice rather than
something that happens to be true of the current default feature set, since
"rustls only" is a hard constraint checked at Milestone Validation, not just
WP1. Re-run the `cargo tree` grep for `openssl`/`native-tls`/`tokio` after
adding these, same as WP1's AC2 check.

### `src/cli.rs` + `src/bin/codemason.rs`

`clap::Command` builder tree: root `codemason` with three subcommands.
`run`'s `--task` accepts `TEXT|@FILE` — a leading `@` means "read task text
from this file path" (resolved relative to the current working directory,
consistent with `--models-config`/`--log`, not the target `--repo`, since the
task file is the operator's, not the target repository's).

`--base-url` / `--api-key` env fallbacks: `CODEMASON_BASE_URL` /
`CODEMASON_API_KEY`. Not named in SPEC.md — inference, flagged for sign-off.
(Not `OPENAI_*`: the base URL is explicitly arbitrary-provider, and reusing
OpenAI's own env var names on a non-OpenAI endpoint would be misleading.)

`main()` stays thin: build the CLI, parse, dispatch to `config`/`gating`/
`index`, map the `Result` to one of the six exit codes via
`std::process::exit`. Exit-code mapping lives in `src/cli.rs` next to the
subcommand definitions (`enum ExitCode` mirroring SPEC's table), not
scattered across call sites.

`run`'s WP2 body: parse flags → resolve `models.toml` via `config::resolve`
→ resolve `--model` (or the allowlist's first/default entry) → resolve
`base_url`/`api_key` (flag, else env, else error) → `gating::check(...)` →
on `Err`, print the rejection reason to stderr, exit 4 → on `Ok`, print
`"model {id} passed gating; execution loop is not implemented until WP3"` to
stderr, exit 0. `--dry-run` etc. are parsed and stored on the config struct,
unused this package — still listed in `--help` (AC1 needs every documented
flag to appear, not to do anything yet).

### `src/config.rs`

```rust
pub struct ModelEntry { pub id: String, pub role: String }
pub struct GatingConfig {
    pub min_context_length: u64,
    pub require_tool_support: bool,
    pub allow_unlisted: bool,
    pub deny_id_patterns: Vec<String>,   // new field, see Ambiguity 1
}
pub struct ModelsConfig { pub models: Vec<ModelEntry>, pub gating: GatingConfig }

pub fn resolve(explicit: Option<&Path>) -> Result<(PathBuf, ModelsConfig), Error>;
```

Resolution: if `--models-config` is given, use exactly that path — error
naming it if missing/malformed, no fallback (an explicit path is the operator
pointing at a specific file; silently falling through past it would hide a
typo). Otherwise try `./models.toml`, then
`dirs::config_dir()/codemason/models.toml` in order; first one that exists
wins. If none exist, exit 1 listing every path that was tried (AC3). Malformed
TOML at whichever path was chosen: exit 1 naming the file path and the
`toml` crate's parse error (line/column included, since `toml`'s error type
carries it).

### `src/gating.rs`

```rust
pub struct CatalogueEntry {
    pub id: String,
    pub context_length: u64,
    pub supported_parameters: Vec<String>,
}

pub fn fetch_catalogue(base_url: &str, api_key: &str) -> Result<Vec<CatalogueEntry>, Error>;
// GET {base_url}/models, Authorization: Bearer {api_key}, JSON `{"data": [...]}`
// (OpenAI/OpenRouter list-endpoint convention — SPEC doesn't name the path,
// flagged for sign-off same as WP1's package-naming inference).

pub struct GateRejection { pub reason: String }

pub fn check(
    id: &str,
    allow_unlisted: bool,
    allowlist: &[ModelEntry],
    gating: &GatingConfig,
    catalogue: &[CatalogueEntry],
) -> Result<(), GateRejection>;
```

Cache: `dirs::cache_dir()/codemason/catalogue/<sanitized-base-url>.json`,
storing `{fetched_at: RFC3339, entries: [...]}` via `serde`. 24h TTL measured
against `fetched_at`. A fetch failure with a valid (even if expired-by-clock
but present) cache entry is non-fatal — spec says "non-fatal only with a
valid cache entry," read as *present*, not as *unexpired*, since the whole
point of falling back is the network being unavailable. Sanitization is a
simple non-alphanumeric-to-`_` pass on the base URL string, just enough to
be a legal filename — not a hash, so the cache file stays human-inspectable.

### `models.toml` (sample, repo root)

```toml
# Placeholder ids. Real ids MUST be validated with `codemason models --check`
# before use — this file is not fetched or trusted blindly.

[[model]]
id = "REPLACE_ME/example-primary-model"
role = "primary"

[[model]]
id = "REPLACE_ME/example-fallback-model"
role = "fallback"

[gating]
min_context_length = 32000
require_tool_support = true
allow_unlisted = false
# deny_id_patterns extends the built-in router/auto-select denylist
# (openrouter/auto, ids ending "/auto", ids containing "auto-router").
deny_id_patterns = []
```

### `src/error.rs` additions

New variants: `ConfigNotFound { searched: Vec<PathBuf> }`,
`ConfigParse { path: PathBuf, source: toml::de::Error }`,
`CatalogueFetch(ureq::Error)`, `ModelGated(GateRejection)`,
`MissingCredential(&'static str)` (for absent `--api-key`/`--base-url` with
no env fallback). No reference to any external crate's exit-code type, per
T1.2's rule carried forward.

## Test strategy, by acceptance criterion

- **AC1** `Command::new(env!("CARGO_BIN_EXE_codemason")).arg("--help")` —
  assert stdout contains `run`, `models`, `index`, and every documented flag
  string.
- **AC2** Integration test: `codemason index --repo <fixture> --stats`
  against the same `old_source` C# fixture WP1 used (same on-disk-only
  caveat), assert exit 0 and plausible fields in the printed stats.
- **AC3** Unit tests in `config.rs`: well-formed file → ordered `Vec`
  matching file order; syntactically broken TOML → `Err` naming file + parse
  error; no file at any searched location → `Err` listing all searched
  paths.
- **AC4** Manual — run by hand once real credentials are available, result
  recorded directly in the AC table.
- **AC5–AC8** Local stub server (`TcpListener`, hand-rolled minimal HTTP,
  same approach WP3 will reuse) serving a fabricated `/models` catalogue per
  scenario (no `tools` in `supported_parameters`; id absent entirely; id
  matching a router pattern; id absent from `models.toml` with/without
  `--allow-unlisted-model`). Drive each through `codemason run`, assert the
  exit code, and assert the stub's request log contains only the `GET
  /models` call — no completions-shaped `POST`, satisfying the package
  gate's explicit demand to prove that rather than infer it from the exit
  code alone.
- **AC9** Same stub, request counter. Two `codemason run` invocations
  against the same `--base-url` inside the TTL: assert exactly one `GET
  /models` total across both.

## Risk flags

1. **`clap` builder API, not `derive`** — SPEC.md T2.1 says "builder API"
   explicitly; noting it because `derive` is the more common default choice
   and I want the constraint on record before writing code against it.
2. **`CODEMASON_BASE_URL`/`CODEMASON_API_KEY` env var names** are an
   inference, not a spec quote.
3. **Catalogue endpoint path** (`{base_url}/models`, OpenAI/OpenRouter list
   convention) is an inference — SPEC.md names the *response fields*
   (`context_length`, `supported_parameters`) but never the path.
4. **`deny_id_patterns` is a new `models.toml` field** beyond T2.2's literal
   three (`min_context_length`, `require_tool_support`, `allow_unlisted`) —
   product of resolving Ambiguity 1 above, flagged again here since it
   changes the shipped sample file's shape.
5. **Cache file location/format/sanitization scheme** under
   `dirs::cache_dir()` is an inference — SPEC.md specifies the 24h TTL and
   the non-fatal-with-valid-cache behavior but not the storage mechanism.
6. **AC4 is not automated** — needs a live credential this sandbox doesn't
   have; will be exercised manually once provided and reported as an actual
   run result, not skipped silently.
7. **`toml` crate version** is unpinned in this plan pending `cargo add`'s
   resolution at implementation time — consistent with how WP1 handled
   already-pinned engine deps versus this package's genuinely new ones.
