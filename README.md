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

Milestone 1, not started. See `SPEC.md` for scope, work packages and acceptance
criteria, and `CLAUDE.md` for how work proceeds in this repository.

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
