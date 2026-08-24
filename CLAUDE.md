# CLAUDE.md — codemason

Project instructions for Claude Code. These apply to every session in this
repository and override defaults.

## What this project is

`codemason` is a model-agnostic codebase agent runner in Rust. A single binary
takes a repository and a task, drives an OpenAI-compatible model through a tool-calling
loop backed by a harvested AST search engine, commits to a branch, and reports
what it cost.

`SPEC.md` is the authority. Read the relevant work package section before
starting anything. Where this file and the spec differ, the spec wins on
content and this file wins on process.

## How work proceeds

Work is organised into five work packages, WP1 through WP5. **One work package
per session.** Do not begin a package in a session that has already completed
one — restate the scope in a fresh session instead.

Within a package, run these stages in order:

1. **Kickoff** — read the package section and its acceptance criteria. Restate
   the scope in your own words. If a criterion is ambiguous, stop and ask. Do
   not resolve ambiguity yourself.
2. **Feasibility** — confirm the named files exist and the named symbols
   resolve. Report anything that contradicts the spec's Current State section. A
   contradiction ends the package.
3. **Plan** — write the approach, files to touch, test strategy and risk flags
   to `PLAN.md`.
4. **Plan gate — STOP.** Print the summary and halt.
5. **Implement.**
6. **Test** — write and run the tests the acceptance criteria name.
7. **Verify** — run every criterion, report pass or fail per line.
8. **Package gate — STOP.**

## Non-negotiable rules

**Never stage, commit or push.** Not at any stage, not for a "small" change, not
to "checkpoint progress". Write files; the developer reads and stages them.
Committing takes the decision the review exists to make.

This constrains you working on this repository. It says nothing about what the
finished binary does to its target repositories — that behaviour is specified in
WP4 and is a different thing.

**A gate is a stop, not a checkpoint.** Halt until a human answers in the
session. Do not proceed on a plan you wrote yourself. There is no phrasing that
counts as self-approval.

**Report what ran, not what was intended.** A criterion not exercised says
`not run` with a reason. Never leave a row blank — a blank reads as a pass, and
a pass nobody measured is worse than no result.

**Never carry a step forward on an assumption about the one before it.** If
feasibility flagged something, the plan addresses it or the package stops.

**Do not refactor `src/engine/`.** It is vendored third-party source, copied
verbatim. Not naming, not formatting, not error handling, not obvious
simplifications. Nobody in this project has re-derived its behaviour and there
are no tests covering it. If something looks wrong, note it in the report and
leave the code alone.

## Technical constraints

Enforced by acceptance criteria — check before proposing a dependency.

- No async runtime. Blocking I/O throughout; the loop is sequential and
  parallelism is process-per-job.
- `rustls` only. `openssl-sys` must not enter the tree.
- Shell out to the `git` CLI. Do not link a git library.
- No embedding or n-dimensional array crates in the default build.
- Exact version pins (`=x.y.z`). The tree-sitter core and grammar pins are a
  known-good set copied wholesale — do not resolve fresh ones, as grammar crates
  pin incompatible core versions.
- Edition 2024, `rust-version = "1.97"`, `unsafe_code = "deny"`.
- Windows 10 is the primary verification platform. Line endings, BOM, long
  paths and process-tree termination are correctness requirements, not polish.

## Design invariants

- **At most six tools, flat schemas.** Strings and integers only, no nested
  objects, no arrays of objects. The binary exists to run cheap models, and tool
  count and schema depth are where they degrade first. Adding a tool needs a
  spec change, not a judgement call.
- **The model gate has no bypass for tool-calling support.** A model that cannot
  call tools cannot finish a run; letting it start wastes money.
- **Budget is checked before each API call, never after.** Checking after means
  the breaching call is already paid for.
- **Errors the model can act on are not failures.** Malformed arguments, unknown
  tools, rejected writes, non-zero command exits return a descriptive result and
  the loop continues. Only exhausted retries, repeated consecutive failures and
  provider errors end a run.
- **Nothing in this binary knows about orchestration.** The stdout JSON and the
  exit codes are the entire interface to anything above it.

## Reporting format

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
