use std::time::Instant;

use codemason_core::cli::{self, ExitCode, RunArgs, API_KEY_ENV, BASE_URL_ENV};
use codemason_core::log::{event_type, EventLog};
use codemason_core::report::{finish, IndexReport, RunReport, TotalsReport};
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

    let index = match Index::build(&args.repo) {
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

    if !args.dry_run {
        let branch = args
            .branch
            .clone()
            .unwrap_or_else(|| format!("codemason/{run_id}"));
        if let Err(err) = repo::create_branch(&args.repo, &branch) {
            eprintln!("error: {err}");
            return finish(report, ExitCode::UnrecoverableError, start);
        }
        report.branch = Some(branch);
    }

    let client = llm::Client::new(base_url, api_key);
    let loop_cfg = LoopConfig {
        repo_root: args.repo.clone(),
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
        match repo::commit_all(&args.repo, &commit_message) {
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

    ExitCode::Completed
}
