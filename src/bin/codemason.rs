use codemason_core::cli::{self, ExitCode, RunArgs, API_KEY_ENV, BASE_URL_ENV};
use codemason_core::{config, gating, Error, Index};

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
    let args = match RunArgs::from_matches(sub) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };

    let (config_path, models_config) = match config::resolve(args.models_config.as_deref()) {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
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
            return ExitCode::UnrecoverableError;
        }
    };

    let base_url = match cli::resolve_credential(args.base_url.as_deref(), BASE_URL_ENV) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::UnrecoverableError;
        }
    };
    let api_key = match cli::resolve_credential(args.api_key.as_deref(), API_KEY_ENV) {
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

    if let Err(rejection) = gating::check(
        &model_id,
        args.allow_unlisted_model,
        &models_config.models,
        &models_config.gating,
        &catalogue,
    ) {
        eprintln!("model rejected: {rejection}");
        return ExitCode::ModelGated;
    }

    eprintln!(
        "model {model_id} passed gating; execution loop is not implemented until WP3"
    );
    ExitCode::Completed
}

fn models_cmd(sub: &clap::ArgMatches) -> ExitCode {
    let (config_path, models_config) = match config::resolve(None) {
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
