# ORCHESTRATION.md — the supervisor above codemason

A design for a vendor-neutral supervisor that decomposes a specification into
work packages, dispatches them to `codemason` processes in parallel, and
integrates the result.

`codemason` itself knows nothing about any of this and must not. The stdout
JSON and the exit codes are the entire interface between the two — see
`SPEC.md`, "Nothing in this binary knows about orchestration".

## Provenance of this design

Two existing systems, combined because they solve different halves of the
problem:

- **SWE-AF** (`github.com/Agent-Field/SWE-AF`, Apache-2.0, Public Beta) —
  supplies the *scheduling* model: a dependency DAG topologically sorted into
  levels, concurrent execution within a level, a hard gate between levels, and
  one git worktree per unit of work. Verified against the primary repository
  and README.
- **Co-Coder** — supplies the *partitioning* model: a static-analysis
  dependency graph partitioned by cohesion, with high in-degree files isolated
  so that file ownership between concurrent workers is disjoint.

The combination is deliberate. SWE-AF detects file-ownership overlap at plan
time and then **passes it to a merger instead of using it to partition**;
Co-Coder's measured results say that is the wrong way round. Where the two
disagree, this design follows Co-Coder on partitioning and SWE-AF on
scheduling.

**Evidence status.** SWE-AF's characterisation above is verified against
primary sources. The comparative numbers in the appendix are extracted from
sources but **were not adversarially verified** — the verification pass was cut
short deliberately. Treat them as strong leads, not settled fact, and re-check
any number before it justifies significant work.

---

## The pipeline

```
1. ANALYZE     dependency graph out of the target repository
2. PARTITION   graph -> disjoint file-ownership groups
3. PLAN        work items -> partitions -> DAG -> levels
4. EXECUTE     per level: N concurrent codemason runs, isolated
5. INTEGRATE   merge branches, run the tests, bounded fix loop
6. REPORT      aggregate cost, status and provenance
```

Each stage below states what it consumes, what it produces, where it came
from, and what the MVP actually builds versus defers.

---

## 1. ANALYZE

**In:** a repository path. **Out:** a directed file dependency graph.

`codemason` already builds this graph — the vendored engine resolves imports
across fifteen languages and exposes `depends_on` per file, `dependents()` for
reverse edges, and `impact()` for transitive reach. `DependencyGraph` and
`FileNode` already derive `Serialize`.

The only addition required is an export:

```
codemason index --repo <PATH> --graph
```

emitting

```json
{"files": {"services/orders/src/OrderService.cs": {
    "depends_on": ["shared/Money.cs"], "symbols": [...]}, ...}}
```

This is a **subcommand flag, not a model-facing tool**. The seven-tool cap in
`SPEC.md` governs what the model sees; the orchestrator surface is not
constrained by it. Nothing in `src/engine/` is modified.

**MVP:** build this. It is small and everything downstream depends on it.

---

### Greenfield repositories have no graph

A skeleton — specifications, ADRs, scaffolding, no source yet — produces an
empty dependency graph, and `--partition` reports
`sequential_reason: "empty"`. That is not "too coupled to split"; it is
"nothing to analyse yet", and the two call for opposite responses.

**On a greenfield repository the architecture document is the partition.** A
specification that lists projects and states their dependencies is a
hand-authored dependency DAG, and a better one than anything inferrable from an
empty tree. Derive levels from the stated dependencies, run the first level to
create the contracts, then re-run `--partition` — from the second level onward
there is code, and stages 1-2 apply normally.

Two rules bind harder here than anywhere else. Contracts must complete before
implementations start, because two jobs inventing the same interface in
parallel will not agree and nothing catches it until integration. And once
contracts exist, their signatures must be pasted verbatim into later task text:
a job cannot search for a file another job wrote in a separate worktree.

### Documentation is indexed

The index includes markdown, YAML, TOML and JSON alongside source, because a
repository's specifications are frequently the only statement of intent that
exists — and on a spec-driven repository they are the entire input. HTML is
indexed as source and always was.

Measured cost: on a code-heavy repository this is roughly +15% files, +35%
chunks and +50% index time, which stays well inside the threshold recorded in
the README. `Index::build_with(path, false)` restores code-only indexing.

Note that indexing a document is not the same as the agent obeying it. A
target repository's own agent rules (`AI_GUIDELINES.md`, `CONTRIBUTING.md`, a
conventions file) are searchable but not automatically loaded, so the task text
must tell the job to read them.

## 2. PARTITION

**In:** the dependency graph. **Out:** disjoint groups of files that can be
worked on concurrently without colliding.

From Co-Coder. The goal is not "split the work evenly" — it is "make file
ownership disjoint so that two workers cannot edit the same file".

Algorithm:

1. Build the undirected co-dependency graph from `depends_on`.
2. Compute in-degree ratio per file from `dependents()`.
3. **Isolate high in-degree files as singletons.** A file that many others
   depend on (Co-Coder uses in-degree ratio > 0.4) is the file most likely to
   be edited by two workers at once. It gets its own partition and is never
   co-scheduled.
4. Group the remainder by cohesion — files that depend on each other belong
   together.
5. Consolidate hub-adjacent leftovers into a single integration group.

Implemented as `codemason index --repo <PATH> --partition [--json]`
(`--hub-ratio` tunes step 3, default `0.10`).

It lives in the binary rather than in a sidecar script in another language.
The deploy target is a container carrying `git` and nothing else, so a Python
or Node partitioner would not run there; "single self-contained binary" is a
constraint this project holds itself to, and the graph is already in memory in
Rust, so serialising it out and back would be work done to no end.

**MVP simplification:** skip Infomap community detection. Use connected
components plus the in-degree singleton isolation in step 3. That is where
most of the benefit is, at a fraction of the code. Add real community
detection only if measurement says the partitions are too coarse.

One deliberate detail: edges *through* a hub are dropped when building
adjacency. Two files related only because both import a shared types module
are not coupled to each other, and treating them as coupled collapses the
whole repository into a single partition.

**Degrade to sequential, deliberately.** If partitioning yields one group, the
repository is too densely coupled to parallelise. Run it as a single
sequential job. This is a correct outcome, not a failure — Co-Coder reports the
same behaviour, and the evidence says naive parallelism on coupled code is
worse than not parallelising at all.

**For microservices, this stage is nearly free.** Service boundaries already
are the cohesion partition. Start there.

---

## 3. PLAN

**In:** the specification's work items plus the partitions. **Out:** a DAG of
issues, topologically sorted into levels.

From SWE-AF.

1. Decompose the specification into discrete work items. Each item names its
   target repository (or monorepo section) and its acceptance check.
2. Assign each item to a partition. **An item spanning two partitions is a
   planning error** — either split it or merge those partitions.
3. Build the dependency DAG between items.
4. Topologically sort with Kahn's algorithm into **levels**. Every item in a
   level is independent of every other item in that level.
5. A cycle is a hard failure. Do not attempt to break it automatically.

**MVP simplification:** start with two levels — independent work, then
integration. Multi-level DAGs are a generalisation to add once the loop works
end to end.

### Item size is a measured trade-off

Two costs pull in opposite directions, and both have numbers:

| too many items | too few items |
|---|---|
| coordination overhead, more branches to merge, more boundaries to disagree at | prompt cost grows **quadratically** with iteration count |

History is append-only and re-sent whole on every call. A 21-iteration run
billed 411,469 prompt tokens for a conversation whose final size was 33,678 —
92% re-sent history. Three seven-iteration jobs cost dramatically less than
one twenty-one-iteration job doing the same work.

**Rule of thumb: an item should complete in roughly 5-10 tool-using
iterations.** Cannot see how it finishes in that many? Split it. Two items
that each take two steps? Merge them.

`--keep-recent-turns` (default 3) elides stale tool results from what is sent
and recovers part of the overhead directly; see SPEC.md's amendment. It
reduces the penalty but does not remove it, so item sizing still matters.

### Write task text that makes the job act, not explore

The dominant observed failure is a job that reads exhaustively and never
writes. One run made 31 `read_file` calls, zero `write_file` calls, and
exhausted its budget having produced nothing. The identical task — with the
contracts pasted into the task text and an explicit "write code early, read at
most 2 files" instruction — committed 14 files.

Discovery the planner has already done must not be paid for again by every
job. Paste in the interfaces and constants, name the two or three files worth
reading, and say not to explore past them.

**The context bottleneck lives here.** `codemason` accepts exactly one input
channel for context: the `--task` string, plus whatever is on disk in the
repository it is pointed at. Everything the planner knows — the interface a
sibling service will expose, the contract a consumer must satisfy — has to be
written into that task text. A worker cannot discover it, because the worker
only ever sees its own repository. Contract-first decomposition is therefore
not a stylistic preference; it is forced by the interface.

---

## 4. EXECUTE

**In:** one level of the DAG. **Out:** one branch per item.

From SWE-AF, and this maps directly onto what `codemason` already does.

For every item in the level, concurrently:

```
codemason run --repo <path> --worktree \
              --task "<the work item, with contracts made explicit>" \
              --budget-tokens N --max-iterations 40
```

Then a **hard gate**: wait for every item in the level before starting the
next. Do not pipeline across levels — that is the ordering guarantee the DAG
exists to provide.

`--worktree` is required whenever two concurrent runs could share a clone.
Without it they share one HEAD and one index, and the failure is not a crash:
the surviving run commits the other's in-flight edits and **reports a branch
that does not hold its commit**, which feeds the supervisor false data. See
`SPEC.md`, "Amendment: worktree isolation and the run summary".

Convergent design worth noting: SWE-AF independently arrived at one git
worktree per unit of work.

**Concurrency limit.** Each process builds its index in memory. Size the
per-host job count against that, not against CPU count alone.

---

## 5. INTEGRATE

**In:** the branches produced by a level. **Out:** a merged, *tested* base.

Both systems agree on the shape, and the evidence says this is the
highest-value component in the whole design: uncoordinated parallel agents
amplify errors ~17x over a single agent, while a central integrator contains
that to ~4x.

1. Merge each branch into the integration base, in level order.
2. **Run the target repository's own test suite.** This is the gate.
3. On failure, localise to the owning partition and re-dispatch a bounded fix
   job — SWE-AF caps this at 2 cycles, which is a sane default.
4. The fixer must never be permitted to silence or delete tests.

**Never gate on `summary`.** The `summary` field is the model's own account of
what it did. It is useful for a human deciding whether to look closer, and
worth logging. It is not evidence. `files_changed`, `commit`, and the exit
status of the test suite are what actually happened.

**MVP simplification — no LLM merge resolution.** Use plain `git merge` and
fail loudly on conflict. The measured conflict rates in the appendix are for
agent PRs against arbitrary repositories on a moving mainline; well-partitioned
microservices should conflict far less. **Measure your own rate first.** If it
is low, failing loudly and re-dispatching is cheaper and far more verifiable
than a merge agent whose output nobody can check.

**MVP simplification — integrate locally, not through pull requests.** SWE-AF
opens a PR and polls GitHub Actions until checks are conclusive. That is a
reasonable end state, but it is deliberately out of scope for the MVP: it binds
the supervisor to one forge, adds minutes of latency per level, and tests
nothing that running the suite locally does not. Merge into a local integration
branch and run the tests directly. PR creation, review gating and CI polling
are a later layer, added once the merge-and-verify loop is proven.

### Supervisor decision rule

Derived from `codemason`'s exit-code contract. The exit code says *how the run
stopped*, not *whether the work is done* — the two are genuinely different.

| exit | meaning | action |
|---|---|---|
| 0 | completed | run the tests; accept or re-dispatch |
| 2 | budget exceeded — **work is committed** | run the tests. Pass: accept. Fail: re-dispatch with remaining budget |
| 3 | iteration ceiling — **work is committed** | as above |
| 1 | unrecoverable | escalate. Dirty tree, bad config, worktree failure — retrying will not fix it |
| 4 | model gated | escalate. Allowlist problem; retrying spends money for nothing |
| 5 | provider error | retry with backoff, then escalate |

Rows 2 and 3 are the ones that matter and the ones a naive supervisor gets
wrong. Observed in a real run: the model completed the task correctly, kept
re-reading files instead of stopping, and exited 3. Partial work was committed
and was correct. Treating exit 3 as failure would have discarded it.

---

## 6. REPORT

**In:** one JSON object per run. **Out:** one aggregate record per build.

Every `codemason` run emits exactly one JSON object on stdout containing
`run_id`, `status`, `exit_code`, `summary`, `branch`, `commit`,
`files_changed`, `iterations`, `models_used`, `totals` (prompt/completion/total
tokens and cost), `duration_ms`, and `log_path`.

The supervisor aggregates:

- **Cost** — sum `totals` across runs. Cost is reported by the provider or is
  absent; nothing is estimated. Attribute per item, per level, and per build.
- **Provenance** — item to branch to commit. `branch` resolves to `commit`,
  which is the property worktree isolation exists to preserve.
- **Outcome** — exit code plus the integration test result. Record both: they
  answer different questions.
- **Diagnostics** — `log_path` points at an append-only JSONL event log that
  survives worktree teardown, because it is written under the original
  repository rather than the worktree.

Reconciling reported totals against the sum of `llm_call` events in the event
log is a cheap and worthwhile invariant check. It has been verified to hold
exactly on a real run.

---

## Build order

1. ~~`--graph` export and a partitioner that prints proposed partitions.~~
   **Done** — `codemason index --repo <PATH> --graph` and
   `--partition [--json]`.
2. Executor: one level, N concurrent `codemason run --worktree`, collect JSON.
3. Integrator: merge, run tests, apply the decision rule above.
4. Planner: specification to DAG. Deliberately last — it is the least certain
   part, and the three stages below it are useful without it.

A first Claude Code front-end over stages 2–4 lives in `.claude/`:
`skills/codemason-orchestrate` (entry point and the rules), the
`codemason-planner` subagent (stage 3), and the `codemason-build` workflow
(stages 4–5 fan-out and integration). It is one front-end over the binary, not
the design — anything that can run a process and read JSON can drive the same
stages.

Start with **multi-repo microservices**, not monorepo sections. Service
boundaries are already the cohesion partition, which means stage 2 is nearly
free and the case the evidence is most pessimistic about is avoided until the
loop is proven.

Explicitly deferred, in rough order of when they become worth adding:

| deferred | why not now |
|---|---|
| Pull requests, review gating, CI polling | binds the supervisor to one forge; local merge-and-test proves the loop first |
| LLM merge resolution | unverifiable output; measure the real conflict rate before building it |
| Infomap community detection | connected components plus in-degree isolation captures most of the benefit |
| Multi-level DAGs | two levels prove the gate; generalise afterwards |
| Monorepo section splitting | works today via `--worktree`, but it is the case the evidence is least kind to |

---

## Appendix: the evidence

**Not adversarially verified** — extracted from sources during a research pass
that was stopped before verification completed. Re-check before relying on any
of it.

Against naive parallelism:

- Across four agentic benchmarks and five architectures, multi-agent systems
  showed no average gain over single-agent baselines; aggregate mean −3.5%.
- Coordination overhead measured at 1.6x–6x the resources of a single
  sequential agent.
- Orchestrated parallel coding agents scored the **lowest** pass rate of any
  method tested on CodeProjectEval (16.3%) — below the sequential single-agent
  baseline — attributed to uncoordinated generation producing conflicting
  interfaces.
- Naive file-level parallel decomposition produced no wall-clock benefit while
  inflating API cost 44%.
- Dependency-heavy planning tasks degrade worst: −70% versus a single agent.
- Uncoordinated parallel agents amplify errors 17.2x; centralised coordination
  contains it to 4.4x.

For cohesion-aware partitioning:

- Co-Coder beat both a sequential baseline and a naive file-based parallel
  baseline: DevEval 68.1% vs 56.8%, with 45% lower latency and 28% lower cost;
  CodeProjectEval 34.1% vs 20.1% sequential and 23.3% file-based.
- Self-reported limit: densely coupled repositories collapse to a single
  partition and degrade to sequential execution.

Conflict base rates:

- 41.7% of cross-agent co-active PR pairs conflicted textually, vs 19.8%
  intra-agent. Caveat: only 0.5% of co-active pairs were cross-agent, so this
  rests on a small subpopulation.
- 27.67% of 107K+ agent PRs across 59K+ repositories conflicted textually,
  averaging ~11 conflict regions per conflicting PR.
- 84.4% of conflicted files were source code rather than dependency manifests;
  ~42% of conflicts were structural (modify/delete, add/add).
- **No study measured semantic conflicts** — changes that merge cleanly and
  break behaviour. That evidence base does not exist yet, which is the single
  largest open risk in this design.

On SWE-AF's own evidence: one self-run, self-scored benchmark on a single
Node.js todo app, scoring 95/100 against 73 for single-agent Claude Code
Sonnet. Not blinded, no independent replication, and the margin came
predominantly from "Structure" and "Git hygiene" — categories that flatter an
orchestrator by construction. Borrow the algorithm; do not treat the benchmark
as support for it.
