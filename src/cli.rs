use std::path::PathBuf;

use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::error::Error;

/// Exit-code contract from SPEC.md WP2/T2.1. A future orchestrator dispatches
/// on these — the values are load-bearing, not incidental.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Completed = 0,
    UnrecoverableError = 1,
    BudgetExceeded = 2,
    MaxIterationsExceeded = 3,
    ModelGated = 4,
    ProviderError = 5,
}

impl From<ExitCode> for i32 {
    fn from(code: ExitCode) -> Self {
        code as i32
    }
}

pub fn build() -> Command {
    Command::new("codemason")
        .about("Model-agnostic codebase agent runner")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(run_command())
        .subcommand(models_command())
        .subcommand(index_command())
}

fn run_command() -> Command {
    Command::new("run")
        .about("Drive a model through a tool-calling loop against a repository")
        .arg(
            Arg::new("repo")
                .long("repo")
                .value_name("PATH")
                .required(true),
        )
        .arg(
            Arg::new("task")
                .long("task")
                .value_name("TEXT|@FILE")
                .required(true),
        )
        .arg(Arg::new("model").long("model").value_name("ID"))
        .arg(
            Arg::new("models-config")
                .long("models-config")
                .value_name("PATH"),
        )
        .arg(Arg::new("base-url").long("base-url").value_name("URL"))
        .arg(Arg::new("api-key").long("api-key").value_name("KEY"))
        .arg(
            Arg::new("budget-tokens")
                .long("budget-tokens")
                .value_name("N")
                .default_value("200000"),
        )
        .arg(Arg::new("budget-usd").long("budget-usd").value_name("USD"))
        .arg(
            Arg::new("max-iterations")
                .long("max-iterations")
                .value_name("N")
                .default_value("40"),
        )
        .arg(Arg::new("branch").long("branch").value_name("NAME"))
        .arg(Arg::new("log").long("log").value_name("PATH"))
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("allow-unlisted-model")
                .long("allow-unlisted-model")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(ArgAction::SetTrue),
        )
}

fn models_command() -> Command {
    Command::new("models")
        .about("List the model allowlist, optionally checking it against the live catalogue")
        .arg(
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue),
        )
}

fn index_command() -> Command {
    Command::new("index")
        .about("Build the search index and report")
        .arg(
            Arg::new("repo")
                .long("repo")
                .value_name("PATH")
                .required(true),
        )
        .arg(
            Arg::new("stats")
                .long("stats")
                .action(ArgAction::SetTrue),
        )
}

#[derive(Debug, Clone)]
pub struct RunArgs {
    pub repo: PathBuf,
    pub task: String,
    pub model: Option<String>,
    pub models_config: Option<PathBuf>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub budget_tokens: u64,
    pub budget_usd: Option<f64>,
    pub max_iterations: u32,
    pub branch: Option<String>,
    pub log: Option<PathBuf>,
    pub dry_run: bool,
    pub allow_unlisted_model: bool,
    pub verbose: bool,
}

impl RunArgs {
    pub fn from_matches(matches: &ArgMatches) -> Result<Self, Error> {
        let raw_task = matches
            .get_one::<String>("task")
            .expect("required")
            .clone();
        Ok(Self {
            repo: PathBuf::from(matches.get_one::<String>("repo").expect("required")),
            task: resolve_task_text(&raw_task)?,
            model: matches.get_one::<String>("model").cloned(),
            models_config: matches
                .get_one::<String>("models-config")
                .map(PathBuf::from),
            base_url: matches.get_one::<String>("base-url").cloned(),
            api_key: matches.get_one::<String>("api-key").cloned(),
            budget_tokens: matches
                .get_one::<String>("budget-tokens")
                .expect("has default")
                .parse()
                .unwrap_or(200_000),
            budget_usd: matches
                .get_one::<String>("budget-usd")
                .and_then(|s| s.parse().ok()),
            max_iterations: matches
                .get_one::<String>("max-iterations")
                .expect("has default")
                .parse()
                .unwrap_or(40),
            branch: matches.get_one::<String>("branch").cloned(),
            log: matches.get_one::<String>("log").map(PathBuf::from),
            dry_run: matches.get_flag("dry-run"),
            allow_unlisted_model: matches.get_flag("allow-unlisted-model"),
            verbose: matches.get_flag("verbose"),
        })
    }
}

/// `--task TEXT|@FILE` — a leading `@` means "read the task text from this
/// file", resolved relative to the current working directory (the
/// operator's, not the target `--repo`).
fn resolve_task_text(raw: &str) -> Result<String, Error> {
    match raw.strip_prefix('@') {
        Some(path) => {
            let path = PathBuf::from(path);
            std::fs::read_to_string(&path).map_err(|source| Error::TaskFileRead { path, source })
        }
        None => Ok(raw.to_string()),
    }
}

/// Resolve a credential from an explicit CLI value, falling back to the
/// named environment variable. Env var names (`CODEMASON_BASE_URL`,
/// `CODEMASON_API_KEY`) are an inference — SPEC.md says "env fallbacks"
/// without naming them. See PLAN.md risk flag 2.
pub fn resolve_credential(explicit: Option<&str>, env_var: &'static str) -> Result<String, Error> {
    if let Some(value) = explicit {
        return Ok(value.to_string());
    }
    std::env::var(env_var).map_err(|_| Error::MissingCredential(env_var))
}

pub const BASE_URL_ENV: &str = "CODEMASON_BASE_URL";
pub const API_KEY_ENV: &str = "CODEMASON_API_KEY";
