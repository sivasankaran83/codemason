use std::time::Instant;

use codemason_core::cli::{self, ExitCode, RunArgs, API_KEY_ENV, BASE_URL_ENV};
use codemason_core::log::{event_type, EventLog};
use codemason_core::report::{finish, IndexReport, RunReport, TotalsReport};
use codemason_core::partition::{self, PartitionOptions};
use codemason_core::text::normalize_slashes;
use codemason_core::{config, gating, llm, repo, Error, Index, LoopConfig, LoopExit};
use uuid::Uuid;

fn main() {
    let matches = cli::build().get_matches();

    let exit_code = match matches.subcommand() {
        Some(("run", sub)) => run_cmd(sub),
        Some(("models", sub)) => models_cmd(sub),
        Some(("index", sub)) => index_cmd(sub),
        _ => unreachable!("clap enforces subcommand_required"),
    };

    std::process::exit(exit_code.into());
}

fn run_cmd(sub: &clap::ArgMatches) -> ExitCode {
    let start = Instant::now();
    let run_id = Uuid::now_v7();
    let mut report = RunReport::new(run_id);

    let args = match RunArgs::from_matches(sub) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };

    // Preflight before any network call — a dirty worktree (or not a git
    // worktree at all) is refused here, before config resolution or gating
    // ever spend anything. `--dry-run` skips this entirely.
    if let Err(err) = repo::preflight(&args.repo, args.dry_run) {
        eprintln!("error: {err}");
        return finish(report, ExitCode::UnrecoverableError, start);
    }

    let (config_path, models_config) = match config::resolve(args.models_config.as_deref()) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };

    let model_id = match args
        .model
        .clone()
        .or_else(|| models_config.default_model().map(|m| m.id.clone()))
    {
        Some(id) => id,
        None => {
            eprintln!(
                "error: no --model given and {} has no [[model]] entries",
                config_path.display()
            );
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };

    let base_url = match cli::resolve_credential(args.base_url.as_deref(), BASE_URL_ENV) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };
    let api_key = match cli::resolve_credential(args.api_key.as_deref(), API_KEY_ENV) {
        Ok(key) => key,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };

    let catalogue = match gating::catalogue(&base_url, &api_key) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::ProviderError, start);
        }
    };

    if let Err(rejection) = gating::check(
        &model_id,
        args.allow_unlisted_model,
        &models_config.models,
        &models_config.gating,
        &catalogue,
    ) {
        eprintln!("model rejected: {rejection}");
        return finish(report, ExitCode::ModelGated, start);
    }

    let log_path = args
        .log
        .clone()
        .unwrap_or_else(|| args.repo.join(".agent").join("log").join(format!("run-{run_id}.jsonl")));
    report.log_path = Some(log_path.to_string_lossy().into_owned());
    let mut event_log = match EventLog::open(&log_path, run_id) {
        Ok(log) => log,
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };
    event_log.write(
        event_type::RUN_STARTED,
        serde_json::json!({
            "repo": args.repo.to_string_lossy(),
            "model": model_id,
            "task_chars": args.task.len(),
        }),
    );

    // Everything from here operates on `work_repo`, which is `args.repo`
    // unless the run is worktree-isolated, in which case it is the same
    // section resolved inside a private checkout. `args.repo` is still what
    // the event log above is anchored to — that is deliberate, and it is
    // what keeps the log readable after the worktree is torn down.
    //
    // The worktree has to exist before the index is built: the index records
    // file paths, and the tools resolve paths against the loop's repo root.
    // Building from one root and resolving against another would put those
    // two out of step.
    let mut work_repo = args.repo.clone();
    let mut worktree: Option<repo::Worktree> = None;
    let branch = args
        .branch
        .clone()
        .unwrap_or_else(|| format!("codemason/{run_id}"));

    if !args.dry_run {
        if args.worktree {
            let at = std::env::temp_dir().join(format!("codemason-wt-{run_id}"));
            match repo::worktree_add(&args.repo, &branch, &at) {
                Ok(wt) => {
                    work_repo = wt.work_path.clone();
                    worktree = Some(wt);
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    return finish(report, ExitCode::UnrecoverableError, start);
                }
            }
        } else if let Err(err) = repo::create_branch(&args.repo, &branch) {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
        report.branch = Some(branch.clone());
    }

    let index = match Index::build(&work_repo) {
        Ok(index) => index,
        Err(Error::IndexBuild(err)) => {
            eprintln!("error: failed to build index: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
        Err(err) => {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
    };
    event_log.write(
        event_type::INDEX_BUILT,
        serde_json::json!({
            "indexed_files": index.stats().indexed_files,
            "total_chunks": index.stats().total_chunks,
            "build_ms": index.stats().build_ms,
        }),
    );
    report.index = Some(IndexReport {
        chunk_count: index.stats().total_chunks,
        build_ms: index.stats().build_ms,
    });

    let client = llm::Client::new(base_url, api_key);
    let loop_cfg = LoopConfig {
        repo_root: work_repo.clone(),
        task: args.task.clone(),
        model: model_id.clone(),
        max_iterations: args.max_iterations,
        budget_tokens: args.budget_tokens,
        budget_usd: args.budget_usd,
        dry_run: args.dry_run,
    };

    let (exit, ledger) = codemason_core::run_loop(&loop_cfg, &client, &index, &mut event_log);

    report.models_used = vec![model_id];
    report.totals = TotalsReport {
        prompt_tokens: ledger.totals().values().map(|t| t.prompt_tokens).sum(),
        completion_tokens: ledger.totals().values().map(|t| t.completion_tokens).sum(),
        total_tokens: ledger.total_tokens(),
        cost: ledger.total_cost(),
    };

    if !args.dry_run {
        let commit_message = commit_message_for(&args.task);
        let committed = repo::commit_all(&work_repo, &commit_message);

        // Tear the worktree down whether or not the commit succeeded — the
        // commit lives on the branch, which outlives the tree, and a
        // worktree left behind on an error path leaks a directory per run.
        if let Some(wt) = &worktree {
            if let Err(err) = repo::worktree_remove(wt) {
                // Not fatal: the work is committed to a branch that is
                // already durable. Report it and carry on rather than
                // failing a run that actually succeeded.
                eprintln!("warning: could not remove worktree: {err}");
            }
        }

        match committed {
            Ok(Some(info)) => {
                report.commit = Some(info.sha);
                report.files_changed = info.files_changed;
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("error: {err}");
                return finish(report, ExitCode::UnrecoverableError, start);
            }
        }
    }

    match exit {
        LoopExit::Completed { summary, iterations } => {
            report.iterations = iterations;
            if args.verbose {
                eprintln!("completed after {iterations} iteration(s)");
            }
            eprintln!("{summary}");
            // Also carried in the stdout report: a supervisor reading only
            // stdout should not have to scrape stderr to learn what the run
            // says it did.
            report.summary = Some(summary);
            finish(report, ExitCode::Completed, start)
        }
        LoopExit::ProviderError { reason, iterations } => {
            report.iterations = iterations;
            eprintln!("provider error after {iterations} iteration(s): {reason}");
            finish(report, ExitCode::ProviderError, start)
        }
        LoopExit::BudgetExceeded { iterations } => {
            report.iterations = iterations;
            eprintln!("budget exceeded after {iterations} iteration(s)");
            finish(report, ExitCode::BudgetExceeded, start)
        }
        LoopExit::MaxIterationsExceeded { iterations } => {
            report.iterations = iterations;
            eprintln!("max iterations ({}) exceeded", args.max_iterations);
            finish(report, ExitCode::MaxIterationsExceeded, start)
        }
    }
}

/// A short, fixed-format commit message — not spec'd beyond "commit... if
/// something changed"; the task text is truncated so a long task doesn't
/// produce an unreadable one-line commit summary.
fn commit_message_for(task: &str) -> String {
    const MAX_CHARS: usize = 72;
    let first_line = task.lines().next().unwrap_or(task);
    let truncated: String = first_line.chars().take(MAX_CHARS).collect();
    if first_line.chars().count() > MAX_CHARS {
        format!("codemason: {truncated}…")
    } else {
        format!("codemason: {truncated}")
    }
}

fn models_cmd(sub: &clap::ArgMatches) -> ExitCode {
    let explicit = sub
        .get_one::<String>("models-config")
        .map(std::path::PathBuf::from);
    let (config_path, models_config) = match config::resolve(explicit.as_deref()) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };

    for model in &models_config.models {
        println!("{}\t{}", model.id, model.role);
    }

    if !sub.get_flag("check") {
        return ExitCode::Completed;
    }

    let base_url = match cli::resolve_credential(None, BASE_URL_ENV) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };
    let api_key = match cli::resolve_credential(None, API_KEY_ENV) {
        Ok(key) => key,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };

    let catalogue = match gating::catalogue(&base_url, &api_key) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::ProviderError;
        }
    };

    let mut all_passed = true;
    for model in &models_config.models {
        match gating::check(
            &model.id,
            false,
            &models_config.models,
            &models_config.gating,
            &catalogue,
        ) {
            Ok(()) => println!("PASS\t{}", model.id),
            Err(rejection) => {
                all_passed = false;
                println!("FAIL\t{}\t{rejection}", model.id);
            }
        }
    }

    if all_passed {
        ExitCode::Completed
    } else {
        eprintln!("error: {} has entries that fail gating", config_path.display());
        ExitCode::ModelGated
    }
}

fn index_cmd(sub: &clap::ArgMatches) -> ExitCode {
    let repo = sub.get_one::<String>("repo").expect("required");
    let stats_requested = sub.get_flag("stats");

    let index = match Index::build(repo) {
        Ok(index) => index,
        Err(Error::IndexBuild(err)) => {
            eprintln!("error: failed to build index: {err}");
            return ExitCode::UnrecoverableError;
        }
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };

    if stats_requested {
        let stats = index.stats();
        println!("indexed_files: {}", stats.indexed_files);
        println!("total_chunks: {}", stats.total_chunks);
        println!("build_ms: {}", stats.build_ms);
        for (language, count) in &stats.languages {
            println!("  {language}: {count}");
        }
    }

    if sub.get_flag("graph") {
        // Only the edges an orchestrator partitions on. The engine's
        // `FileNode` also carries symbols and raw imports; emitting those
        // would multiply the payload for data no partitioner reads, and this
        // is written for repositories large enough that the difference
        // matters.
        let graph = index.graph();
        let mut files = serde_json::Map::new();
        for path in graph.all_files() {
            let depends_on: Vec<&str> = graph
                .deps(&path)
                .map(|node| node.depends_on.iter().map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let dependents = graph.dependents(&path);
            files.insert(
                normalize_slashes(&path),
                serde_json::json!({
                    "depends_on": depends_on.iter().map(|p| normalize_slashes(p)).collect::<Vec<_>>(),
                    // Pre-computed because in-degree is what decides which
                    // files must never be co-scheduled, and every consumer
                    // would otherwise invert the edge set to get it.
                    "dependent_count": dependents.len(),
                }),
            );
        }

        let payload = serde_json::json!({
            "repo": normalize_slashes(&repo.to_string()),
            "file_count": graph.file_count(),
            "edge_count": graph.edge_count(),
            "files": files,
        });
        let line = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        if let Err(err) = codemason_core::text::write_stdout(&format!("{line}\n")) {
            eprintln!("error: could not write graph: {err}");
            return ExitCode::UnrecoverableError;
        }
    }

    if sub.get_flag("partition") {
        let opts = PartitionOptions {
            hub_ratio: sub
                .get_one::<String>("hub-ratio")
                .and_then(|s| s.parse().ok())
                .unwrap_or(PartitionOptions::default().hub_ratio),
            ..PartitionOptions::default()
        };
        let result = partition::partition(index.graph(), opts);

        if sub.get_flag("json") {
            let line = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
            if let Err(err) = codemason_core::text::write_stdout(&format!("{line}\n")) {
                eprintln!("error: could not write partitions: {err}");
                return ExitCode::UnrecoverableError;
            }
        } else {
            let s = &result.stats;
            println!(
                "files {}  edges {}  ->  {} parallel partition(s) + {} hub(s)",
                s.files, s.edges, s.component_partitions, s.hub_partitions
            );
            println!();
            if s.degrades_to_sequential {
                if s.sequential_reason == Some("empty") {
                    println!("NO DEPENDENCY GRAPH: nothing here to partition.");
                    println!("The engine found no files it builds a graph for. Usually this");
                    println!("means the repository is documentation, configuration or shell");
                    println!("scripts rather than source in a language it parses -- not that");
                    println!("the code is coupled. Orchestration does not apply here.");
                } else {
                    println!("DEGRADES TO SEQUENTIAL: too densely coupled to split usefully.");
                    println!("Run it as a single job. Per ORCHESTRATION.md this is a correct");
                    println!("outcome, not a failure -- naive parallelism on coupled code");
                    println!("measures worse than not parallelising at all.");
                }
                println!();
            }
            for p in &result.partitions {
                if p.kind == "hub" {
                    println!(
                        "  {:>4}  HUB   {}  ({} dependents)",
                        p.id,
                        p.files[0],
                        p.dependent_count.unwrap_or(0)
                    );
                } else {
                    let head: Vec<&str> = p
                        .files
                        .iter()
                        .take(4)
                        .map(|f| f.rsplit('/').next().unwrap_or(f))
                        .collect();
                    let more = if p.file_count > 4 {
                        format!(" +{} more", p.file_count - 4)
                    } else {
                        String::new()
                    };
                    println!("  {:>4}  [{:>3}] {}{}", p.id, p.file_count, head.join(", "), more);
                }
            }
            println!();
            println!("Hubs are single-file partitions: editable, never by two jobs at once.");
        }
    }

    ExitCode::Completed
}
