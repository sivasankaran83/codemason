export const meta = {
  name: 'codemason-build',
  description: 'Analyze, partition, plan, dispatch parallel codemason runs, then merge and verify',
  whenToUse: 'When the user asks to run codemason across a large codebase or several microservices in parallel. Spawns many agents and spends real money — only on explicit request.',
  phases: [
    { title: 'Analyze', detail: 'dependency graph + cohesion partitions' },
    { title: 'Plan', detail: 'goal -> self-contained work items, one per partition' },
    { title: 'Execute', detail: 'one isolated codemason run per work item, concurrently' },
    { title: 'Integrate', detail: 'merge branches, run the test suite' },
  ],
}

// Stages follow ORCHESTRATION.md. The deterministic parts (partitioning,
// dispatch, aggregation) are code; only planning and integration diagnosis
// are model judgement. That split is the point: partitioning by model would
// be non-reproducible and worse.

const repo = (args && args.repo) || '.'
const goal = (args && args.task) || ''
const budget = (args && args.budgetTokens) || 200000
const maxIterations = (args && args.maxIterations) || 40
const model = (args && args.model) || null
const dryRun = !!(args && args.dryRun)

if (!goal) {
  throw new Error('codemason-build requires args.task — the goal to decompose')
}

const PARTITIONS = {
  type: 'object',
  required: ['usableParallelism', 'degradesToSequential', 'partitions'],
  properties: {
    usableParallelism: { type: 'integer' },
    degradesToSequential: { type: 'boolean' },
    fileCount: { type: 'integer' },
    hubs: { type: 'array', items: { type: 'string' } },
    partitions: {
      type: 'array',
      items: {
        type: 'object',
        required: ['id', 'kind', 'fileCount'],
        properties: {
          id: { type: 'string' },
          kind: { type: 'string' },
          fileCount: { type: 'integer' },
          sampleFiles: { type: 'array', items: { type: 'string' } },
        },
      },
    },
  },
}

const PLAN = {
  type: 'object',
  required: ['items', 'levels', 'rationale'],
  properties: {
    levels: { type: 'integer' },
    rationale: { type: 'string' },
    notParallelized: { type: 'string' },
    items: {
      type: 'array',
      items: {
        type: 'object',
        required: ['id', 'level', 'repo', 'task'],
        properties: {
          id: { type: 'string' },
          partitionId: { type: 'string' },
          level: { type: 'integer' },
          repo: { type: 'string' },
          task: { type: 'string' },
          acceptance: { type: 'string' },
        },
      },
    },
  },
}

const RUN = {
  type: 'object',
  required: ['exitCode', 'status'],
  properties: {
    exitCode: { type: 'integer' },
    status: { type: 'string' },
    branch: { type: 'string' },
    commit: { type: 'string' },
    filesChanged: { type: 'array', items: { type: 'string' } },
    totalTokens: { type: 'integer' },
    cost: { type: 'number' },
    summary: { type: 'string' },
    note: { type: 'string' },
  },
}

const INTEGRATION = {
  type: 'object',
  required: ['merged', 'testsPassed'],
  properties: {
    merged: { type: 'array', items: { type: 'string' } },
    conflicts: { type: 'array', items: { type: 'string' } },
    testsPassed: { type: 'boolean' },
    testCommand: { type: 'string' },
    failureSummary: { type: 'string' },
    failingPartition: { type: 'string' },
    // Reported per attempt so the fix loop can tell a shrinking error set from
    // a shifting one. A shifting one does not converge.
    errorCount: { type: 'integer' },
    errorCodes: { type: 'array', items: { type: 'string' } },
    integrationBranch: { type: 'string' },
  },
}

phase('Analyze')
log(`repo: ${repo}`)

const partitions = await agent(
  `Run these two commands in \`${repo}\` and report the result:\n\n` +
    `1. \`codemason index --repo ${repo} --partition\`\n` +
    `2. \`codemason index --repo ${repo} --partition --json\`\n\n` +
    `Return the stats and a compact summary of each partition (id, kind, fileCount, ` +
    `and up to 5 sample files). Do NOT return every file path — large repositories ` +
    `produce thousands and they are not needed downstream.\n\n` +
    `If the command fails, say exactly how it failed. Do not invent partitions.`,
  { label: 'partition', phase: 'Analyze', schema: PARTITIONS },
)

if (!partitions) {
  return { error: 'partitioning failed; nothing dispatched' }
}

log(`${partitions.fileCount || '?'} files -> ${partitions.usableParallelism} usable partition(s)`)

// Honest stop. Splitting a densely coupled repository measures worse than not
// splitting it, so the correct output here is a recommendation, not a fan-out.
if (partitions.degradesToSequential) {
  log('degrades to sequential — not fanning out')
  return {
    decision: 'sequential',
    reason:
      'Partitioning found no independent groups: this repository is too densely ' +
      'coupled to split. Run a single codemason job instead. Naive parallelism on ' +
      'coupled code measures worse than sequential execution (ORCHESTRATION.md).',
    partitions,
  }
}

phase('Plan')

const plan = await agent(
  `Goal:\n${goal}\n\nRepository: ${repo}\n\n` +
    `Partitions (file ownership is disjoint between them):\n` +
    JSON.stringify(partitions.partitions, null, 1) +
    `\n\nHub files (high in-degree; at most ONE item per level may touch a given hub):\n` +
    JSON.stringify(partitions.hubs || [], null, 1) +
    `\n\nProduce codemason work items. Rules, in order of importance:\n` +
    `- One item maps to exactly ONE partition. An item spanning partitions is a planning error.\n` +
    `- Each item's \`task\` text must be COMPLETELY self-contained. The job sees only its own ` +
    `repository and that text — it cannot see sibling jobs or discover a contract defined ` +
    `elsewhere. Write out any interface, signature or constant it must match, verbatim.\n` +
    `- Items in the same level run concurrently. Put a dependent item in a later level.\n` +
    `- Prefer fewer, larger items. Do not manufacture parallelism; if the goal touches one ` +
    `cohesive area, emit a single item and say so in \`notParallelized\`.\n\n` +
    `Investigate the repository first (Grep/Read) so the task text names real files and real ` +
    `signatures rather than guesses.`,
  { label: 'plan', phase: 'Plan', schema: PLAN, agentType: 'codemason-planner' },
)

if (!plan || !plan.items || plan.items.length === 0) {
  return { error: 'planning produced no work items', partitions }
}

log(`${plan.items.length} item(s) across ${plan.levels} level(s)`)

// Group by level. The hard gate between levels is the ordering guarantee the
// whole design rests on — never pipeline across it.
const byLevel = new Map()
for (const item of plan.items) {
  const l = item.level || 1
  if (!byLevel.has(l)) byLevel.set(l, [])
  byLevel.get(l).push(item)
}
const levels = [...byLevel.keys()].sort((a, b) => a - b)

phase('Execute')
const runs = []

for (const level of levels) {
  const items = byLevel.get(level)
  log(`level ${level}: dispatching ${items.length} job(s)`)

  const results = await parallel(
    items.map((item) => () =>
      agent(
        `Run exactly one codemason job and report its JSON output verbatim.\n\n` +
          `Command:\n` +
          `codemason run --repo ${item.repo} --worktree \\\n` +
          `  --task ${JSON.stringify(item.task)} \\\n` +
          `  --budget-tokens ${budget} --max-iterations ${maxIterations}` +
          (model ? ` \\\n  --model ${model}` : '') +
          (dryRun ? ` \\\n  --dry-run` : '') +
          `\n\n` +
          `--worktree is REQUIRED: without it two concurrent jobs sharing a clone ` +
          `corrupt each other and misreport which branch holds their commit.\n\n` +
          `codemason writes exactly one JSON object to stdout. Report its fields as-is. ` +
          `Do NOT edit any file yourself, do not fix anything, do not re-run on failure. ` +
          `A non-zero exit is a result to report, not a problem to solve — exit 2 and 3 ` +
          `mean partial work WAS committed.`,
        { label: `run:${item.id}`, phase: 'Execute', schema: RUN },
      ).then((r) => (r ? { ...r, itemId: item.id, level, repo: item.repo } : null)),
    ),
  )

  runs.push(...results.filter(Boolean))
}

const dispatched = runs.length
const committed = runs.filter((r) => r.commit)

phase('Integrate')

// A work item gets at most this many fix cycles before a human sees it.
// Measured: three cycles on one item cost roughly $0.14 and never converged —
// version-less package references (NU1015), then a NuGet package the model
// invented and which does not exist (NU1101), then 36 code errors that had
// been hidden behind the restore failure. The error set changed shape each
// cycle instead of shrinking, which is the signature of a root cause that
// re-dispatching cannot reach — here, weak model knowledge of an external
// framework. Cycles past the second spend money moving the errors around.
const MAX_FIX_CYCLES = 2

function integrationPrompt(cycle) {
  return (
    `Integrate these codemason branches in repository ${repo}.\n\n` +
    JSON.stringify(
      committed.map((r) => ({ item: r.itemId, level: r.level, repo: r.repo, branch: r.branch, commit: r.commit })),
      null,
      1,
    ) +
    (cycle ? `\n\nThis is the re-check after fix cycle ${cycle} of ${MAX_FIX_CYCLES}.` : '') +
    `\n\nSteps:\n` +
    `1. Create an integration branch from the current base.\n` +
    `2. Merge each branch in LEVEL ORDER. Record any that conflict — do NOT resolve ` +
    `conflicts by hand or by rewriting code; report them and stop merging that branch.\n` +
    `3. Run the repository's own test suite (find it: cargo test, npm test, go test, dotnet test).\n` +
    `4. Report whether it passed, and if not, which partition the failure belongs to ` +
    `(\`failingPartition\`).\n` +
    `5. Report \`errorCount\` — how many distinct errors the build or test run printed — and ` +
    `\`errorCodes\`, their identifiers (NU1101, CS0246, failing test names). Report what the ` +
    `tool printed, not a summary of it: these decide whether a fix cycle is converging.\n\n` +
    `Do NOT open a pull request and do not wait on CI — that layer is deliberately ` +
    `out of scope.\n` +
    `Do NOT silence, skip or delete tests under any circumstances. If tests fail, that ` +
    `is the finding.`
  )
}

// Converging means the same errors, fewer of them. An attempt that introduces
// error kinds the previous one did not have has moved the error set rather
// than shrunk it, and further cycles will not close it.
function converging(before, after) {
  if (!before || !after) return false
  const b = before.errorCodes || []
  const a = after.errorCodes || []
  if (a.some((code) => !b.includes(code))) return false
  if (typeof before.errorCount === 'number' && typeof after.errorCount === 'number') {
    return after.errorCount < before.errorCount
  }
  return a.length < b.length
}

let integration = committed.length
  ? await agent(integrationPrompt(0), { label: 'integrate', phase: 'Integrate', schema: INTEGRATION })
  : null

const fixRuns = []
const fixCycles = []
let stoppedConverging = false

while (integration && integration.testsPassed === false && fixCycles.length < MAX_FIX_CYCLES) {
  const cycle = fixCycles.length + 1
  const before = integration
  log(`integration tests failed — fix cycle ${cycle} of ${MAX_FIX_CYCLES}`)

  const fix = await agent(
    `Dispatch exactly ONE bounded codemason fix job. This is fix cycle ${cycle} of ` +
      `${MAX_FIX_CYCLES}; after the cap the item goes to a human, not to another cycle.\n\n` +
      `Integration tests failed. Failure:\n${before.failureSummary || '(none reported)'}\n\n` +
      `Errors reported: ${JSON.stringify(before.errorCodes || [])}\n` +
      `Owning partition: ${before.failingPartition || 'unknown — localise it from the failure above'}\n\n` +
      `Write the --task text yourself from that failure. It must name the exact files and the ` +
      `exact errors, and nothing beyond them.\n\n` +
      `Command shape:\n` +
      `codemason run --repo ${repo} --worktree \\\n` +
      `  --task "<the fix task you wrote>" \\\n` +
      `  --budget-tokens ${budget} --max-iterations ${maxIterations}` +
      (model ? ` \\\n  --model ${model}` : '') +
      (dryRun ? ` \\\n  --dry-run` : '') +
      `\n\n` +
      `If the fix needs an external package, put the exact package id and a real version in ` +
      `the task text — a job left to guess a package name will invent one that does not ` +
      `exist. Instruct it that a package which fails to restore must be removed and reported, ` +
      `not replaced with another guess.\n\n` +
      `Do NOT edit any file yourself and do NOT silence, skip or delete tests. Report the ` +
      `codemason JSON verbatim.`,
    { label: `fix:${cycle}`, phase: 'Integrate', schema: RUN },
  )

  if (fix) fixRuns.push({ ...fix, itemId: `fix:${cycle}`, cycle, repo })

  integration = await agent(integrationPrompt(cycle), {
    label: `integrate:${cycle}`,
    phase: 'Integrate',
    schema: INTEGRATION,
  })

  if (integration && integration.testsPassed === false && !converging(before, integration)) {
    stoppedConverging = true
  }

  fixCycles.push({
    cycle,
    failureBefore: before.failureSummary || null,
    errorCountBefore: typeof before.errorCount === 'number' ? before.errorCount : null,
    errorCountAfter: integration && typeof integration.errorCount === 'number' ? integration.errorCount : null,
    fix: fix ? { exitCode: fix.exitCode, branch: fix.branch || null, commit: fix.commit || null } : null,
    testsPassed: !!(integration && integration.testsPassed),
  })

  // Shape change rather than shrinkage: the remaining cycle would spend money
  // moving the errors around. Escalate now instead of using it up.
  if (stoppedConverging) {
    log('error set changed shape rather than shrinking — escalating instead of retrying')
    break
  }
}

const needsHuman = !!(integration && integration.testsPassed === false)
if (needsHuman) {
  log(`unresolved after ${fixCycles.length} fix cycle(s) — escalate to a human`)
}

const totals = [...runs, ...fixRuns].reduce(
  (acc, r) => ({
    tokens: acc.tokens + (r.totalTokens || 0),
    cost: acc.cost + (r.cost || 0),
  }),
  { tokens: 0, cost: 0 },
)

// Exit 2 and 3 committed work; whether they are "done" is decided by the tests,
// not by the exit code. Surface them so a human can judge rather than folding
// them into a pass/fail.
const partial = runs.filter((r) => r.exitCode === 2 || r.exitCode === 3)
const escalate = runs.filter((r) => [1, 4, 5].includes(r.exitCode))

return {
  decision: 'parallel',
  goal,
  levels: levels.length,
  dispatched,
  committed: committed.length,
  partitions: {
    usableParallelism: partitions.usableParallelism,
    hubs: partitions.hubs || [],
  },
  plan: { rationale: plan.rationale, notParallelized: plan.notParallelized || null },
  runs: runs.map((r) => ({
    item: r.itemId,
    level: r.level,
    exitCode: r.exitCode,
    status: r.status,
    branch: r.branch || null,
    commit: r.commit || null,
    filesChanged: r.filesChanged || [],
    // Model prose. Log it, never gate on it.
    summary: r.summary || null,
  })),
  partialWorkCommitted: partial.map((r) => ({ item: r.itemId, exitCode: r.exitCode })),
  needsEscalation: escalate.map((r) => ({ item: r.itemId, exitCode: r.exitCode, status: r.status })),
  integration,
  fixCycles,
  fixCycleCap: MAX_FIX_CYCLES,
  escalatedToHuman: needsHuman,
  totals,
  verdict: integration
    ? integration.testsPassed
      ? fixCycles.length
        ? `tests passed after ${fixCycles.length} fix cycle(s)`
        : 'tests passed after integration'
      : stoppedConverging
        ? 'INTEGRATION TESTS FAILED — the error set shifted rather than shrank; escalate to a human'
        : `INTEGRATION TESTS FAILED after ${fixCycles.length} fix cycle(s) at the cap — escalate to a human`
    : 'nothing was committed; no integration attempted',
}
