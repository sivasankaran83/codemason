---
name: codemason-orchestrate
description: Plan and run parallel codemason jobs across a large codebase — analyze the dependency graph, partition it so concurrent agents cannot collide, dispatch isolated codemason runs, merge and verify. Use when asked to run codemason across a large repo or several microservices, to parallelize work over a codebase, or to orchestrate multiple coding agents. Also use to inspect proposed partitions before dispatching anything.
---

# Orchestrating codemason across a large codebase

`codemason` executes one task against one repository and knows nothing about
orchestration — the stdout JSON and the exit codes are its entire interface
(`SPEC.md`). This skill is the layer above it: decide what can safely run in
parallel, dispatch it, and verify the result.

Read `ORCHESTRATION.md` at the repository root for the full design and the
evidence behind it. The short version of that evidence, because it determines
how you should behave:

**Naive parallelism is measurably worse than not parallelising.** Orchestrated
parallel coding agents have scored *below* a sequential single-agent baseline
when work was split without dependency awareness. Uncoordinated agents amplify
errors ~17x; a central integrator contains it to ~4x. Parallelism is only worth
it when file ownership between jobs is disjoint. If it cannot be made disjoint,
running sequentially is the correct answer, not a fallback.

## The stages

Follow `ORCHESTRATION.md`: **Analyze → Partition → Plan → Execute → Integrate
→ Report.** Never skip Partition and never guess at it.

### 0. Which kind of repository is this?

Check before anything else, because it decides where partitions come from:

```bash
codemason index --repo <PATH> --partition
```

- **`NO DEPENDENCY GRAPH`** — a greenfield or skeleton repository: specs and
  scaffolding, little or no source yet. Skip to *Greenfield* below. Do not
  conclude the work cannot be parallelised; there is simply no code to analyse
  yet.
- **`DEGRADES TO SEQUENTIAL`** — real code, one cohesive cluster. Dispatch a
  single job. Say so plainly.
- **Several partitions** — an existing codebase. Continue with stages 1-2.

### Greenfield: partition from the architecture, not from code

When there is no code yet, the dependency graph is empty and the partitioner
has nothing to work with. **The architecture document is the partition.** A
specification that lists projects and states their dependencies is a
hand-authored dependency DAG, and it is more reliable than anything inferred
from an empty tree.

Read the repository's own structure and architecture docs, then derive levels
from the stated dependencies. For example, a spec saying *"Abstractions: no
dependencies, contracts only"* and *"Core: no framework coupling"* gives:

```
level 1  Abstractions          (nothing depends on it yet)
level 2  Core                  (depends on Abstractions)
level 3  Reasoning, Ontology, Context     -- concurrent
level 4  Grains
level 5  Silo, Gateway, Ingestion         -- concurrent
```

Two greenfield-specific rules:

- **Contracts first, always.** The level that defines interfaces must complete
  before anything implementing them starts. A job cannot see its sibling's
  output, so an interface invented in parallel by two jobs will not match.
- **Paste the contract into the task text.** Once level 1 exists, later jobs
  need those signatures written out verbatim in their `--task` text. They
  cannot `context_search` for a file another job just wrote in a different
  worktree.

After the first level lands, the repository has code, and stages 1-2 apply
normally from then on. Re-run `--partition` between levels.

### Respect the target repository's own rules

Many repositories carry instructions for coding agents — `AI_GUIDELINES.md`,
`CONTRIBUTING.md`, `CLAUDE.md`, `AGENTS.md`, a constitution or conventions
file. **codemason does not read these on its own**, and its index does not
chunk markdown, so `context_search` will not surface them either.

Every `--task` text must therefore instruct the job to read them first:

> Before writing any code, read AI_GUIDELINES.md and .harness/conventions.md
> in full and follow them exactly.

Skipping this produces code that is correct and unmergeable.

### 1-2. Analyze and partition

```bash
codemason index --repo <PATH> --partition           # readable summary
codemason index --repo <PATH> --partition --json    # machine-readable
codemason index --repo <PATH> --graph               # raw dependency graph
```

Partitioning is deterministic graph arithmetic, not a judgement call. Do not
substitute your own opinion about what "looks independent" — run it. It lives
in the binary, so it works anywhere `codemason` does, including inside the
container.

Read `stats.usable_parallelism` from the JSON. **If `degrades_to_sequential` is
true, stop and say so**: the repository is too densely coupled to split, and a
single sequential job is the right dispatch. Report that plainly rather than
splitting anyway.

`--hub-ratio` (default `0.10`) tunes how readily a file is treated as a hub.
Raise it to get fewer, larger partitions; lower it to isolate more files.

Hub files (high in-degree, emitted as single-file partitions) are the ones that
collide. They can be edited, but never by two jobs at once.

### 3. Plan

Map the user's task onto partitions. Use the `codemason-planner` subagent for
this — it is the one stage that needs judgement.

Two rules that are not negotiable:

- **A work item spanning two partitions is a planning error.** Split the item
  or merge those partitions. Do not dispatch it as-is.
- **Everything a job needs must be in its `--task` text.** A job sees only its
  own repository. If service A must match an interface owned by service B, that
  interface has to be written into A's task text verbatim — the job cannot
  discover it. Contract-first decomposition is forced by the interface, not a
  style preference.

### 4. Execute

One `codemason` process per work item, concurrently within a level:

```bash
codemason run --repo <PATH> --worktree \
  --task "<complete, self-contained work item>" \
  --budget-tokens 200000 --max-iterations 40
```

`--worktree` is **required** whenever two concurrent jobs could share a clone.
Without it they share one HEAD and one index; the surviving run commits the
other's edits and reports a branch that does not hold its commit, which feeds
you false data. Separate clones do not need it.

Wait for every job in a level before starting the next.

### 5. Integrate

Merge each branch into an integration branch, then **run the repository's own
test suite**. That test result is the gate.

**Never treat `summary` as evidence.** It is the model's own account of what it
did. `files_changed`, `commit`, and the test suite are what actually happened.

On failure, localise to the owning partition and re-dispatch a bounded fix job.
Cap it at 2 cycles. Never let a fix job silence or delete tests.

Do not open pull requests or wait on CI — that layer is deliberately deferred
(`ORCHESTRATION.md`). Merge locally and test locally.

### 6. Report

Each run emits one JSON object. Aggregate `totals` for cost (provider-reported;
never estimate), map item → branch → commit for provenance, and record both the
exit code and the integration test result — they answer different questions.

## Reading exit codes

The exit code says *how a run stopped*, not *whether the work is done*.

| exit | action |
|---|---|
| 0 | run the tests; accept or re-dispatch |
| 2 | budget exceeded — **work is committed.** Run the tests. Pass: accept. Fail: re-dispatch with remaining budget |
| 3 | iteration ceiling — **work is committed.** Same as 2 |
| 1 | escalate — dirty tree, bad config, worktree failure. Retrying will not fix it |
| 4 | escalate — model gating. Retrying spends money for nothing |
| 5 | retry with backoff, then escalate |

Rows 2 and 3 are the ones that matter. In a real run the model completed its
task correctly, kept re-reading files instead of stopping, and exited 3 with the
correct work committed. Treating exit 3 as failure discards good work.

## Running the whole pipeline

For a full build, invoke the workflow rather than driving stages by hand:

```
Workflow({ name: "codemason-build", args: { repo: "<PATH>", task: "<goal>" } })
```

It runs analyze → partition → plan → execute (fan-out) → integrate and returns
the aggregate report. Only use it when the user has asked for multi-agent
orchestration — it spawns many agents and spends real money.

For anything smaller, drive the stages above directly. A single job against a
single repository does not need any of this: just run `codemason` once.
