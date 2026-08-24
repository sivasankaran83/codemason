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
use codemason_core::supervisor::execute::{dispatch_level, DispatchConfig, Execution};
use codemason_core::supervisor::integrate::{
    create_integration_branch, evidence_lines, mergeable, run_acceptance, CompletedItem,
    Integration, NextStep,
};
use codemason_core::supervisor::memory::{BriefOptions, Fact, JsonlMemory, MemoryStore};
use codemason_core::supervisor::plan::{plan as make_plan, PlanRequest};
use codemason_core::supervisor::{Disposition, Plan, WorkItem, MAX_FIX_CYCLES, MAX_REWRITES};
use codemason_core::text::write_stdout;
use uuid::Uuid;

/// Exit codes, deliberately distinct from `codemason`'s so a caller can tell
/// which layer stopped.
const OK: u8 = 0;
const FAILED: u8 = 1;
const ESCALATED: u8 = 10;

/// Diagnostic lines written into the memory when a level is rewritten.
///
/// Enough to name the members the discarded attempt invented, few enough that
/// the brief's character budget still leaves room for the task itself — a
/// task string that is mostly compiler output has crowded out the task.
const MAX_EVIDENCE_LINES: usize = 12;

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
  --planner-model-hard <ID>  planner for a resumed build, where the previous
                           attempt already failed and the memory says why
  --models-config <PATH>   passed through to codemason
  --codemason <PATH>       the codemason binary; default: resolved from PATH
  --budget-tokens <N>      per job
  --max-iterations <N>     per job
  --max-concurrent <N>     jobs in flight at once
  --memory <PATH>          memory JSONL; default <repo>/.agent/supervisor/<id>.jsonl
  --base <BRANCH>          base to integrate onto; default: current branch
  --resume <BUILD-ID>      continue a build: reuse its memory and integration
                           branch, and skip levels already accepted
  --base-url <URL>         provider endpoint; env CODEMASON_BASE_URL
  --api-key <KEY>          provider key; env CODEMASON_API_KEY
  --dry-run                plan and print, dispatch nothing

The planner and the jobs are separate model choices on purpose. Planning and
diagnosis are where judgement lives and where every observed failure came
from; execution is mechanical. A strong planner over cheap workers is the
split this design is for, and planning runs once per build rather than once
per iteration, so it is cheap in absolute terms:

  --planner-model anthropic/claude-sonnet-5 --model minimax/minimax-m2.7

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
    // A resumed build keeps its identity: same memory file, same integration
    // branch. Everything the previous attempt learned is the reason to resume
    // rather than start again, and a new id would orphan it.
    let resumed = match flag(args, "--resume") {
        // An empty id is a caller bug — usually a shell variable that did not
        // expand. Taking it would resume "build-", which is a different build
        // from any that has ever run.
        Some(id) if id.trim().is_empty() => {
            anyhow::bail!("--resume was given an empty build id")
        }
        Some(id) => Some(id.trim().to_string()),
        None => None,
    };
    let build_id = resumed.clone().unwrap_or_else(|| Uuid::now_v7().to_string());

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
    if resumed.is_some() && !memory_path.exists() {
        anyhow::bail!(
            "cannot resume {build_id}: no memory at {}. Without it a resume is just              a rerun that has forgotten what the first attempt learned.",
            memory_path.display()
        );
    }
    let mut memory = JsonlMemory::open(&memory_path)?;

    // Levels an earlier attempt already got past. Recorded as an accepted
    // attempt against a synthetic `level-N` id, so resume reads the same
    // record the loop writes rather than a second bookkeeping file that could
    // disagree with it.
    let done: Vec<u32> = accepted_levels(&memory)?;
    if !done.is_empty() {
        eprintln!(
            "resuming {build_id}: level(s) {} already accepted, skipping",
            done.iter().map(|l| l.to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    let plan = load_plan(args, &repo, &mut memory, resumed.is_some())?;

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
    if resumed.is_some() && branch_exists(&repo, &integration_branch) {
        // Reset onto the existing branch rather than recreating it from base:
        // recreating would discard the levels already merged, which is the
        // work resume exists to keep.
        checkout_existing(&repo, &integration_branch)?;
        eprintln!("resuming on {integration_branch}");
    } else {
        create_integration_branch(&repo, &integration_branch, &base)?;
        eprintln!("integrating onto {integration_branch} (base {base})");
    }

    let mut report = BuildReport {
        build_id: build_id.to_string(),
        base,
        integration_branch: integration_branch.clone(),
        ..Default::default()
    };

    for level in plan.levels() {
        if done.contains(&level) {
            report.levels_accepted += 1;
            continue;
        }
        let items: Vec<_> = plan.items_at(level).into_iter().cloned().collect();
        eprintln!("level {level}: dispatching {} job(s)", items.len());

        let executions = dispatch_level(&items, &dispatch);
        let (branches, escalate) =
            collect_level(level, &items, &executions, &mut memory, &mut report)?;

        if let Some(reason) = escalate {
            return finish(report, NextStep::Escalate, reason, started);
        }

        // Where the integration branch stood before this level merged
        // anything. A rewrite resets to this commit rather than to `base`,
        // so discarding this level's branches leaves the levels already
        // accepted onto the branch exactly where they are.
        let level_base = branch_commit(&repo, &integration_branch)?;
        let mut integration = Integration::new(integration_branch.clone());
        integration.merge_level(&repo, &branches)?;
        report.merged += branches.len() - integration.conflicts.len();

        // Verify. The gate runs on the integrated tree, so it is a property
        // of the level rather than of one item; `--acceptance` overrides, and
        // otherwise the level's own items supply it. A level whose items
        // disagree about how to prove themselves is a planning error, and
        // saying so beats silently picking one.
        let mut item_gates: Vec<String> = items
            .iter()
            .filter_map(|i| i.acceptance.as_deref())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        item_gates.sort();
        item_gates.dedup();

        // `--acceptance` overrides everything. Otherwise EVERY distinct gate
        // the level's items named is run, and all of them must pass.
        //
        // Running only the first was the earlier behaviour and it was wrong
        // in the worst available way: a level of two items with two different
        // per-project build commands reported success having verified one of
        // them. A gate that is not run is not a gate, and a supervisor that
        // reports work it never tested is worse than one with no gate at all,
        // because the report is believed.
        let gates: Vec<String> = match acceptance.clone() {
            Some(global) => vec![global],
            None => item_gates,
        };

        if gates.is_empty() {
            eprintln!(
                "level {level}: merged {} branch(es); no acceptance command, not verified",
                branches.len()
            );
            memory.append(Fact::note(
                None,
                format!("level {level} merged without verification"),
            ))?;
            continue;
        }
        eprintln!("level {level}: {} acceptance command(s) to satisfy", gates.len());

        let mut cycle = 0u32;
        // Rewrites are counted apart from fix cycles because they answer
        // different questions: a fix cycle says the work is sound but
        // incomplete, a rewrite says it was built on a premise that does not
        // hold. One must not consume the other's allowance.
        let mut rewrites = 0u32;
        loop {
            // First failure decides the cycle: a level is only accepted when
            // every one of its gates passes.
            let mut verification = run_acceptance(&repo, &gates[0])?;
            for gate in gates.iter().skip(1) {
                if !verification.passed {
                    break;
                }
                verification = run_acceptance(&repo, gate)?;
            }
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
                NextStep::Rewrite => {
                    rewrites += 1;
                    // The cap is enforced inside `record_verification`; this
                    // is the same belt-and-braces guard as the fix-cycle one
                    // below, so a future change there cannot turn this into
                    // an unbounded spend.
                    if rewrites > MAX_REWRITES {
                        return finish(
                            report,
                            NextStep::Escalate,
                            format!(
                                "level {level} was rewritten {MAX_REWRITES} time(s) and is still \
                                 built on a wrong premise"
                            ),
                            started,
                        );
                    }

                    // What the rewrite has to be told. The codes alone say a
                    // member was not found; the lines say which member, and
                    // that is the fact that stops the next attempt inventing
                    // it again.
                    let evidence = integration
                        .verifications
                        .last()
                        .map(|v| {
                            evidence_lines(
                                &v.output,
                                &decision.novel_error_codes,
                                MAX_EVIDENCE_LINES,
                            )
                        })
                        .unwrap_or_default();
                    memory.append(Fact::learned(
                        None,
                        format!(
                            "level {level} was discarded and written again rather than fixed: {}. \
                             The names these diagnostics report do not exist on the real surface, \
                             so do not call them:\n{}",
                            decision.reason,
                            evidence.join("\n")
                        ),
                    ))?;

                    // A rewrite is only worth dispatching if the *input*
                    // changed, and the only input a job has is its task
                    // string — `dispatch_args` passes `--task` and nothing
                    // else, and the process itself remembers nothing. So the
                    // memory is carried into the task text here; without it
                    // the rewrite reproduces the failure at full price.
                    //
                    // The original task, not a fix task. There is nothing on
                    // those branches worth repairing, which is the whole
                    // finding behind this branch of the decision.
                    let rewritten: Vec<WorkItem> = items
                        .iter()
                        .map(|item| {
                            let brief = memory
                                .brief_for(&item.id, BriefOptions::default())
                                .unwrap_or_default();
                            let mut rewritten = item.clone();
                            if !brief.trim().is_empty() {
                                rewritten.task = format!(
                                    "{}\n\nAn earlier attempt at this exact task was discarded. \
                                     What it got wrong:\n{}",
                                    item.task, brief
                                );
                            }
                            rewritten
                        })
                        .collect();

                    eprintln!(
                        "level {level}: discarding {} branch(es) and dispatching the original \
                         task(s) again",
                        integration.merged.len()
                    );

                    let executions = dispatch_level(&rewritten, &dispatch);
                    let (rewritten_branches, escalate) =
                        collect_level(level, &rewritten, &executions, &mut memory, &mut report)?;
                    if let Some(reason) = escalate {
                        return finish(report, NextStep::Escalate, reason, started);
                    }

                    // Discarding is declining to merge, and nothing needs
                    // reverting: every job commits to its own branch and none
                    // of them ever touches the base, so the discarded work
                    // exists only on branches nobody merges. The integration
                    // branch goes back to where this level found it — which
                    // keeps the levels already accepted onto it — and the
                    // rewritten branches are merged onto that instead. That
                    // is why a rewrite costs one dispatch rather than a
                    // repair.
                    let dropped = integration.discard_merges();
                    report.merged -= dropped.min(report.merged);
                    create_integration_branch(&repo, &integration_branch, &level_base)?;
                    integration.merge_level(&repo, &rewritten_branches)?;
                    report.merged += rewritten_branches.len() - integration.conflicts.len();
                    continue;
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
        // Recorded so a later resume can skip this level. `accepted` is the
        // acceptance command's verdict, which is the only thing entitled to
        // say a level is done.
        memory.append(Fact::attempt(
            &format!("level-{level}"),
            0,
            0,
            true,
            format!("level {level} accepted"),
        ))?;
        report.levels_accepted += 1;
    }

    finish(report, NextStep::Accept, "all levels accepted".to_string(), started)
}

/// Record one level's dispatches and return the branches worth merging,
/// together with a reason to escalate if one of them produced one.
///
/// Lifted out of the level loop because a rewrite dispatches the same level a
/// second time and the bookkeeping is identical either way. A second copy of
/// it would be a second place for the report and the memory to drift apart,
/// and the memory is the only thing a re-dispatched job learns from.
fn collect_level(
    level: u32,
    items: &[WorkItem],
    executions: &[Execution],
    memory: &mut JsonlMemory,
    report: &mut BuildReport,
) -> Result<(Vec<String>, Option<String>), anyhow::Error> {
    let mut completed: Vec<CompletedItem> = Vec::new();
    let mut escalate: Option<String> = None;

    for exec in executions {
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

    let branches = completed
        .iter()
        .filter_map(|c| c.outcome.branch.clone())
        .collect();
    Ok((branches, escalate))
}

fn load_plan(
    args: &[String],
    repo: &Path,
    memory: &mut JsonlMemory,
    is_resume: bool,
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

    // A resumed build is the hard case by definition: the previous plan did
    // not survive verification, and this one is being written against the
    // record of why. That is where a stronger planner earns its cost, and it
    // is a rare enough call that the cost barely registers against the jobs.
    let model = if is_resume {
        flag(args, "--planner-model-hard")
            .or_else(|| flag(args, "--planner-model"))
            .or_else(|| flag(args, "--model"))
    } else {
        flag(args, "--planner-model").or_else(|| flag(args, "--model"))
    }
    .ok_or_else(|| anyhow::anyhow!("--planner-model or --model is required to plan"))?;
    if is_resume {
        eprintln!("replanning with {model}");
    }

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

/// Levels an earlier attempt accepted, read back from the memory the loop
/// itself writes.
fn accepted_levels(memory: &JsonlMemory) -> Result<Vec<u32>, anyhow::Error> {
    let mut levels: Vec<u32> = memory
        .all()?
        .into_iter()
        .filter(|f| f.accepted == Some(true))
        .filter_map(|f| {
            f.item_id
                .as_deref()
                .and_then(|id| id.strip_prefix("level-"))
                .and_then(|n| n.parse::<u32>().ok())
        })
        .collect();
    levels.sort_unstable();
    levels.dedup();
    Ok(levels)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", branch])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn checkout_existing(repo: &Path, branch: &str) -> Result<(), anyhow::Error> {
    let out = std::process::Command::new("git")
        .args(["checkout", "-q", branch])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "could not check out {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The commit a branch points at.
///
/// A rewrite needs the commit the integration branch stood at before the
/// level merged anything, so it can drop that level's branches without
/// disturbing the levels already merged onto it. A branch name would not do:
/// the name is about to be moved.
fn branch_commit(repo: &Path, branch: &str) -> Result<String, anyhow::Error> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(repo)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "could not read the commit of {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
        // A build never finishes on either of these: both mean another
        // dispatch, and the loop only leaves them by accepting or escalating.
        // They are here so the match stays exhaustive rather than guessing.
        NextStep::Redispatch | NextStep::Rewrite => "unresolved",
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
