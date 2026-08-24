//! `codemason-supervisor` — stages 3 to 6 of `ORCHESTRATION.md`, driving
//! `codemason` as a child process.
//!
//! A separate binary rather than a `codemason` subcommand. The whole design
//! rests on `codemason` knowing nothing about orchestration, and a worker that
//! orchestrates itself cannot be composed. This process talks to that one
//! through exactly the published contract — argv in, one JSON object and an
//! exit code out — and reaches inside it nowhere.
//!
//! The loop is AgentFlow's: planner, executor, verifier, generator, with an
//! evolving memory spanning them. Only the architecture is borrowed; nothing
//! here trains a planner, so none of that paper's reported gains are claimed.

use std::path::{Path, PathBuf};
use std::process::ExitCode as ProcExitCode;
use std::time::Instant;

use codemason_core::llm::Client;
use codemason_core::supervisor::execute::{dispatch_level, DispatchConfig};
use codemason_core::supervisor::integrate::{
    create_integration_branch, mergeable, run_acceptance, CompletedItem, Integration, NextStep,
};
use codemason_core::supervisor::memory::{BriefOptions, Fact, JsonlMemory, MemoryStore};
use codemason_core::supervisor::plan::{plan as make_plan, PlanRequest};
use codemason_core::supervisor::{Disposition, Plan, MAX_FIX_CYCLES};
use codemason_core::text::write_stdout;
use uuid::Uuid;

/// Exit codes, deliberately distinct from `codemason`'s so a caller can tell
/// which layer stopped.
const OK: u8 = 0;
const FAILED: u8 = 1;
const ESCALATED: u8 = 10;

fn main() -> ProcExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") || args.is_empty() {
        eprintln!("{USAGE}");
        return ProcExitCode::from(if args.is_empty() { FAILED } else { OK });
    }

    match run(&args) {
        Ok(code) => ProcExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err}");
            ProcExitCode::from(FAILED)
        }
    }
}

const USAGE: &str = "\
codemason-supervisor — plan, dispatch and integrate codemason runs

USAGE:
  codemason-supervisor --repo <PATH> --goal <TEXT|@FILE> [OPTIONS]
  codemason-supervisor --repo <PATH> --plan <FILE.json> [OPTIONS]

  --repo <PATH>            target repository
  --goal <TEXT|@FILE>      what to build; planned by a model
  --plan <FILE>            skip planning and use this plan JSON
  --partitions <FILE>      partitions JSON; default: run codemason itself
  --contracts <FILE>       contract surface to paste into task text
  --acceptance <CMD>       command that proves a level worked
  --model <ID>             model for the jobs
  --planner-model <ID>     model for planning; defaults to --model
  --models-config <PATH>   passed through to codemason
  --codemason <PATH>       the codemason binary; default: resolved from PATH
  --budget-tokens <N>      per job
  --max-iterations <N>     per job
  --max-concurrent <N>     jobs in flight at once
  --memory <PATH>          memory JSONL; default <repo>/.agent/supervisor/<id>.jsonl
  --base <BRANCH>          base to integrate onto; default: current branch
  --dry-run                plan and print, dispatch nothing

Exit: 0 accepted, 1 error, 10 escalated to a human.";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// `@file` means "read the value from this file", matching `codemason run
/// --task`.
fn text_or_file(raw: &str) -> Result<String, anyhow::Error> {
    match raw.strip_prefix('@') {
        Some(p) => Ok(std::fs::read_to_string(p)?),
        None => Ok(raw.to_string()),
    }
}

fn read_optional(args: &[String], name: &str) -> Result<String, anyhow::Error> {
    match flag(args, name) {
        Some(p) => Ok(std::fs::read_to_string(p)?),
        None => Ok(String::new()),
    }
}

fn run(args: &[String]) -> Result<u8, anyhow::Error> {
    let started = Instant::now();
    let build_id = Uuid::now_v7();

    let repo = PathBuf::from(
        flag(args, "--repo").ok_or_else(|| anyhow::anyhow!("--repo is required"))?,
    );
    let dry_run = has(args, "--dry-run");

    // Memory lives under the ORIGINAL repository, never inside a worktree a
    // job might remove. It is the record of what the loop learned; losing it
    // with a torn-down checkout would defeat the point of keeping it.
    let memory_path = flag(args, "--memory").map(PathBuf::from).unwrap_or_else(|| {
        repo.join(".agent")
            .join("supervisor")
            .join(format!("{build_id}.jsonl"))
    });
    let mut memory = JsonlMemory::open(&memory_path)?;

    let plan = load_plan(args, &repo, &mut memory)?;

    eprintln!(
        "plan: {} item(s) across {} level(s)",
        plan.items.len(),
        plan.levels().len()
    );
    if let Some(note) = plan.not_parallelized.as_deref().filter(|s| !s.is_empty()) {
        eprintln!("not parallelised: {note}");
    }

    if dry_run {
        write_stdout(&format!("{}\n", serde_json::to_string(&plan)?))?;
        eprintln!("dry run: nothing dispatched");
        return Ok(OK);
    }

    let acceptance = flag(args, "--acceptance").map(|s| s.to_string());
    let base = match flag(args, "--base") {
        Some(b) => b.to_string(),
        None => current_branch(&repo)?,
    };

    let dispatch = DispatchConfig {
        binary: flag(args, "--codemason")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("codemason")),
        model: flag(args, "--model").map(|s| s.to_string()),
        models_config: flag(args, "--models-config").map(PathBuf::from),
        budget_tokens: flag(args, "--budget-tokens")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DispatchConfig::default().budget_tokens),
        max_iterations: flag(args, "--max-iterations")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DispatchConfig::default().max_iterations),
        max_concurrent: flag(args, "--max-concurrent")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DispatchConfig::default().max_concurrent),
        ..DispatchConfig::default()
    };

    let integration_branch = format!("codemason/build-{build_id}");
    create_integration_branch(&repo, &integration_branch, &base)?;
    eprintln!("integrating onto {integration_branch} (base {base})");

    let mut report = BuildReport {
        build_id: build_id.to_string(),
        base,
        integration_branch: integration_branch.clone(),
        ..Default::default()
    };

    for level in plan.levels() {
        let items: Vec<_> = plan.items_at(level).into_iter().cloned().collect();
        eprintln!("level {level}: dispatching {} job(s)", items.len());

        let executions = dispatch_level(&items, &dispatch);
        let mut completed: Vec<CompletedItem> = Vec::new();
        let mut escalate: Option<String> = None;

        for exec in &executions {
            match &exec.result {
                Err(err) => {
                    // A process that would not start will not start on a
                    // retry either. Straight to a human.
                    memory.append(Fact::note(
                        Some(&exec.item_id),
                        format!("dispatch failed: {err}"),
                    ))?;
                    escalate.get_or_insert(format!("{} could not be dispatched: {err}", exec.item_id));
                }
                Ok(outcome) => {
                    report.totals.add(outcome.totals.total_tokens, outcome.totals.cost);
                    let cycle = memory.cycles_attempted(&exec.item_id)? + 1;
                    memory.append(Fact::attempt(
                        &exec.item_id,
                        cycle,
                        outcome.exit_code,
                        // Not yet verified at this point; the acceptance
                        // command is what decides, and it runs after merge.
                        false,
                        format!(
                            "exit {} ({}), {} file(s) changed",
                            outcome.exit_code,
                            outcome.status,
                            outcome.files_changed.len()
                        ),
                    ))?;

                    if exec.disposition() == Some(Disposition::Escalate) {
                        escalate.get_or_insert(format!(
                            "{} exited {} — retrying spends money without changing anything",
                            exec.item_id, outcome.exit_code
                        ));
                    }
                    if mergeable(outcome) {
                        let item = items
                            .iter()
                            .find(|i| i.id == exec.item_id)
                            .cloned()
                            .expect("every execution came from this level's items");
                        completed.push(CompletedItem {
                            item,
                            outcome: outcome.clone(),
                        });
                    }
                    report.items.push(ItemReport {
                        id: exec.item_id.clone(),
                        level,
                        exit_code: outcome.exit_code,
                        status: outcome.status.clone(),
                        branch: outcome.branch.clone(),
                        commit: outcome.commit.clone(),
                        files_changed: outcome.files_changed.len(),
                        // Model prose. Recorded, never gated on.
                        summary: outcome.summary.clone(),
                    });
                }
            }
        }

        if let Some(reason) = escalate {
            return finish(report, NextStep::Escalate, reason, started);
        }

        let branches: Vec<String> = completed
            .iter()
            .filter_map(|c| c.outcome.branch.clone())
            .collect();
        let mut integration = Integration::new(integration_branch.clone());
        integration.merge_level(&repo, &branches)?;
        report.merged += branches.len() - integration.conflicts.len();

        // Verify. Without an acceptance command there is nothing to gate on,
        // and saying so is more honest than reporting success we did not test.
        let Some(command) = acceptance.as_deref() else {
            eprintln!("level {level}: merged {} branch(es); no --acceptance, not verified", branches.len());
            memory.append(Fact::note(None, format!("level {level} merged without verification")))?;
            continue;
        };

        let mut cycle = 0u32;
        loop {
            let verification = run_acceptance(&repo, command)?;
            memory.append(Fact::errors(
                &format!("level-{level}"),
                cycle,
                format!(
                    "{} error(s): {}",
                    verification.error_count,
                    verification
                        .error_codes
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ))?;

            let decision = integration.record_verification(verification);
            eprintln!("level {level}: {}", decision.reason);

            match decision.step {
                NextStep::Accept => break,
                NextStep::Escalate => {
                    return finish(report, NextStep::Escalate, decision.reason, started)
                }
                NextStep::Redispatch => {
                    cycle += 1;
                    // The cap is enforced inside `record_verification`; this
                    // is a belt-and-braces guard so a future change there
                    // cannot turn this into an unbounded spend.
                    if cycle > MAX_FIX_CYCLES {
                        return finish(
                            report,
                            NextStep::Escalate,
                            format!("level {level} unresolved after {MAX_FIX_CYCLES} fix cycle(s)"),
                            started,
                        );
                    }
                    // Fix authoring is Tier 3 and deliberately not here: every
                    // fix that worked in testing named the root cause, and a
                    // supervisor that re-dispatched "the build failed, fix it"
                    // did not converge. Escalate with the error set recorded so
                    // the human, or a model front-end, has what it needs.
                    return finish(
                        report,
                        NextStep::Escalate,
                        format!(
                            "level {level} failed verification and fix authoring is not automated; \
                             error set recorded in {}",
                            memory_path.display()
                        ),
                        started,
                    );
                }
            }
        }
        report.levels_accepted += 1;
    }

    finish(report, NextStep::Accept, "all levels accepted".to_string(), started)
}

fn load_plan(
    args: &[String],
    repo: &Path,
    memory: &mut JsonlMemory,
) -> Result<Plan, anyhow::Error> {
    if let Some(path) = flag(args, "--plan") {
        let raw = std::fs::read_to_string(path)?;
        return Ok(serde_json::from_str(&raw)?);
    }

    let goal = text_or_file(
        flag(args, "--goal").ok_or_else(|| anyhow::anyhow!("--goal or --plan is required"))?,
    )?;
    let contracts = read_optional(args, "--contracts")?;
    let partitions = match flag(args, "--partitions") {
        Some(p) => std::fs::read_to_string(p)?,
        None => partitions_from_codemason(args, repo)?,
    };

    if !contracts.trim().is_empty() {
        memory.append(Fact::contract(None, "contract surface supplied to the planner"))?;
    }

    let base_url = codemason_core::cli::resolve_credential(
        flag(args, "--base-url"),
        codemason_core::cli::BASE_URL_ENV,
    )?;
    let api_key = codemason_core::cli::resolve_credential(
        flag(args, "--api-key"),
        codemason_core::cli::API_KEY_ENV,
    )?;
    let client = Client::new(base_url, api_key);

    let model = flag(args, "--planner-model")
        .or_else(|| flag(args, "--model"))
        .ok_or_else(|| anyhow::anyhow!("--planner-model or --model is required to plan"))?;

    // The memory is empty on a first build and full on a resumed one. Either
    // way it goes to the planner: that is what makes this a loop rather than
    // a sequence of unrelated dispatches.
    let brief = memory.brief_for("*", BriefOptions::default()).unwrap_or_default();

    make_plan(
        &client,
        &PlanRequest {
            goal: &goal,
            repo: &repo.to_string_lossy(),
            partitions_json: &partitions,
            memory: &brief,
            contracts: &contracts,
            model,
        },
    )
}

fn partitions_from_codemason(args: &[String], repo: &Path) -> Result<String, anyhow::Error> {
    let binary = flag(args, "--codemason").unwrap_or("codemason");
    let out = std::process::Command::new(binary)
        .args(["index", "--repo"])
        .arg(repo)
        .args(["--partition", "--json"])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "codemason index --partition failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn current_branch(repo: &Path) -> Result<String, anyhow::Error> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("could not read the current branch of {}", repo.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Debug, Default, serde::Serialize)]
struct BuildTotals {
    total_tokens: u64,
    cost: f64,
}

impl BuildTotals {
    fn add(&mut self, tokens: u64, cost: f64) {
        self.total_tokens += tokens;
        self.cost += cost;
    }
}

#[derive(Debug, serde::Serialize)]
struct ItemReport {
    id: String,
    level: u32,
    exit_code: i32,
    status: String,
    branch: Option<String>,
    commit: Option<String>,
    files_changed: usize,
    summary: Option<String>,
}

#[derive(Debug, Default, serde::Serialize)]
struct BuildReport {
    build_id: String,
    outcome: String,
    reason: String,
    base: String,
    integration_branch: String,
    levels_accepted: usize,
    merged: usize,
    items: Vec<ItemReport>,
    totals: BuildTotals,
    duration_ms: u128,
}

/// One JSON object on stdout, mirroring `codemason`'s own contract so that
/// whatever sits above this layer reads the same shape from both.
fn finish(
    mut report: BuildReport,
    step: NextStep,
    reason: String,
    started: Instant,
) -> Result<u8, anyhow::Error> {
    report.outcome = match step {
        NextStep::Accept => "accepted",
        NextStep::Redispatch => "unresolved",
        NextStep::Escalate => "escalated",
    }
    .to_string();
    report.reason = reason;
    report.duration_ms = started.elapsed().as_millis();

    write_stdout(&format!("{}\n", serde_json::to_string(&report)?))?;
    Ok(match step {
        NextStep::Accept => OK,
        _ => ESCALATED,
    })
}
