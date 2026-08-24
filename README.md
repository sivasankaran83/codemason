# codemason

Model-agnostic Rust coding agent. Runs one task against one repository,
unattended, with AST-aware retrieval and a hard cost ceiling.

## What it is

`codemason` is a single Rust binary that takes a repository and a task
description, drives an OpenAI-compatible model through a tool-calling loop, and
commits the result to a branch. It reports what it changed and what it cost as
JSON on stdout.

One process handles one repository. Parallelism is more processes, not more
threads — jobs against different repositories share no state, so N containers is
the scaling model.

Retrieval is AST-aware rather than grep-based. That matters more than it sounds:
a strong model compensates for weak search by reading widely and recovering, but
a cheaper model given plain grep burns iterations and hits the budget cap without
finishing. Good retrieval is what makes cheap execution viable.

## Design commitments

- **Any provider.** Any OpenAI-compatible endpoint. No vendor lock in the loop.
- **Curated models only.** A two-layer gate — an operator allowlist plus a
  runtime capability check. A model that does not advertise native tool-calling
  support is refused before the first request, with no bypass flag. A model that
  cannot call tools cannot finish a run, and letting it start wastes money.
- **Hard cost ceiling.** Token and cost budgets are checked before each API
  call, never after. Breaching the budget commits partial work and exits with a
  distinct code rather than failing.
- **Honest reporting.** Cost comes from the provider or is reported as absent.
  Nothing is estimated.
- **No orchestration.** The stdout JSON and the exit codes are the entire
  interface. Anything that plans, retries or fans out sits above this binary.

## Status

Milestone 1, work packages 1 through 5 complete (engine harvest, CLI/config/
gating, the read-only agent loop, writes/commands/git/budget, and the closing
measurement/containerisation/acceptance package). The container in this repo
is written to spec but has not been built or run — this development machine
has neither Docker nor a WSL Linux distribution installed, so that one piece
is unverified; see [Container](#container) below. See `SPEC.md` for scope,
work packages and acceptance criteria, and `CLAUDE.md` for how work proceeds
in this repository.

## Where codemason sits

`codemason` is the **executor**. It runs one task against one repository and
knows nothing about orchestration — the stdout JSON and the exit codes are its
entire interface upward. Everything else in the diagram below belongs to a
supervisor above it.

The supervisor's loop follows AgentFlow (arXiv:2510.05592): four modules —
planner, executor, verifier, generator — coordinated by an evolving memory
across turns. Mapped onto the six pipeline stages:

```
                                        AgentFlow module
1. ANALYZE     dependency graph              --            setup
2. PARTITION   disjoint file ownership       --            setup
   ----------------------------------------------------------------
3. PLAN        work items -> DAG -> levels   planner     \
4. EXECUTE     N concurrent codemason runs   executor     |  the loop
5. INTEGRATE   merge, test, bounded fix      verifier     |
6. REPORT      cost, status, provenance      generator   /
   ----------------------------------------------------------------
            evolving memory spans 3-6 and persists across cycles
```

Stages 1 and 2 run once per build and are already in this binary
(`codemason index --graph` and `--partition`). Stage 4 is `codemason run`
itself. Stages 3, 5 and 6 live above it.

The evolving memory matters because every `codemason` process is stateless by
design: task in, commit, exit, remembers nothing. Without a memory in the
supervisor, two consecutive attempts at a failing item start equally blind.

See `ORCHESTRATION.md` for the full design, what to build and what not to, and
the measured evidence behind both.

## Running jobs in parallel

One process handles one job. How you isolate those processes depends on
whether they share a clone:

| jobs | isolation | flag |
|---|---|---|
| separate clones | already independent | none needed |
| same clone, whole repo | worktree per run | `--worktree` |
| same clone, different sections of a monorepo | worktree per run | `--worktree` |

**Without `--worktree`, two runs must not share a clone.** They share one
HEAD and one index, so they will stage each other's in-flight edits, one will
die on a ref lock, and — worst — the survivor can report a branch that does
not hold its commit. That last part matters because anything automated is
reading that JSON.

```
codemason run --repo /mono/services/orders --worktree --task "..."
```

`--repo` accepts a repository root or any subdirectory of one. Point it at a
subdirectory and the index, the tools and the commit are all scoped to that
section — the agent sees the service it is working on, not the whole
monorepo, which keeps context small enough for a cheap model to be effective.

Each run creates its worktree (~0.2 s, shared object store), works, commits to
its own branch, and removes the tree. The branch stays behind; merging it is
the caller's job. Event logs are written under the original repository, not
the worktree, so they survive teardown.

## Web search

`web_search` is the seventh tool, added by a recorded amendment to `SPEC.md`
after M1 (see "Amendment: the seventh tool" there for the reasoning and the
cost). It is provider-agnostic — no vendor is compiled into the binary:

```
CODEMASON_SEARCH_URL=https://api.search.brave.com/res/v1/web/search
CODEMASON_SEARCH_API_KEY=<key>
CODEMASON_SEARCH_KEY_HEADER=X-Subscription-Token   # optional; this is the default
```

Brave (2,000 queries/month), Tavily (1,000) and Serper (2,500) all have free
tiers. The response is parsed structurally rather than against one vendor's
schema, so any provider returning JSON with title/url/description-shaped
results works.

With no provider configured it falls back to a keyless DuckDuckGo endpoint.
That fallback is **best-effort and will fail intermittently** — the endpoint
rate-limits and answers with an HTTP 202 challenge page rather than results.
Configure a provider for anything that matters.

## The stdout report

Exactly one JSON object, on every exit path; diagnostics go to stderr. This
plus the exit code is the whole interface.

```json
{
  "run_id": "…", "status": "completed", "exit_code": 0,
  "summary": "Added a Discount property to the Line class in src/Order.cs.",
  "branch": "codemason/…", "commit": "…", "files_changed": ["src/Order.cs"],
  "iterations": 5, "index": {"chunk_count": 4, "build_ms": 8},
  "models_used": ["…"],
  "totals": {"prompt_tokens": 620, "completion_tokens": 165,
             "total_tokens": 785, "cost": 0.0},
  "duration_ms": 462, "log_path": "…"
}
```

`summary` is the model's own account of what it did — the message it ended on
— and is `null` on any run that did not complete. Useful for a human deciding
whether to look closer, and worth logging. It is not evidence: `files_changed`
and `commit` are what happened, `summary` is what the model says happened.

`totals.cost` comes from the provider or is zero. Nothing is estimated.

## Exit codes

These are a contract — a supervisor dispatches on them.

| Code | Meaning |
|---|---|
| 0 | Task completed |
| 1 | Unrecoverable error |
| 2 | Budget exceeded — partial work may be committed |
| 3 | Max iterations exceeded — partial work may be committed |
| 4 | Model rejected by gating |
| 5 | Provider error after retries |

## Safety

There is no command allowlist. `run_command` executes what the model asks for.
The isolation boundary is the container and the disposable repository copy — not
a filter the model can talk its way around. Do not point this at a repository
you care about outside a sandbox.

## Vendored source

`src/engine/` is vendored third-party source, copied verbatim. See `ORIGIN.md`
beside it and `LICENSE-ENGINE` at the repository root. It is not refactored,
reformatted or otherwise modified — see `CLAUDE.md`.

## Index cost measurement

Per SPEC.md's WP5/T5.1: `codemason index --stats` measured against three real
repositories of increasing size, including the largest available on the
development machine. "Cold" means the first read of that repository's files
in the measuring process (not a dropped OS file cache, which needs elevated
tooling not available here — the numbers below understate a truly cold read
on a machine that has never touched these files before). "Warm" is an
immediate second run against the same repository.

| repository | indexed files | chunks | cold | warm |
|---|---|---|---|---|
| small (C#) | 18 | 95 | 329 ms | 53 ms |
| medium (Go + C#) | 66 | 121 | 1,198 ms | 126 ms |
| large (Go, Rust, TypeScript, SQL — largest available) | 214 | 879 | 4,503 ms | 1,348 ms |

**Decision: build in-memory per run, do nothing further.** Even the largest
repository available for measurement builds in 4.5 s cold, comfortably inside
T5.1's "under ~5 s" threshold. Index persistence (a versioned on-disk format,
`--out`/`--index` flags) is not worth building for M1 — revisit only if
repositories an order of magnitude larger become the norm, or if parallel job
counts make repeated cold builds visible in practice.

## Container

Multi-stage build: a slim Rust builder compiles the release binary, then a
`debian:bookworm-slim` runtime carries only `git`, CA certificates and the
binary — `git` is required because the runner shells out to it rather than
linking a git library; no model weights ever enter the image, since retrieval
is BM25 + AST only.

```
docker build -t codemason .
docker run --rm -v /path/to/target/repo:/repo codemason \
  run --repo /repo --task "..." --model <id> --base-url <url> --api-key <key>
```
