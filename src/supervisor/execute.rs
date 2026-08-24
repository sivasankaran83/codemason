//! Stage 4, EXECUTE — one level of the plan dispatched as N concurrent
//! `codemason run` processes.
//!
//! The supervisor never reaches inside the binary. It starts a process, reads
//! the single JSON object the process writes to stdout, and reads the exit
//! code; those two things are the whole interface, so this module is almost
//! entirely about not corrupting either of them.
//!
//! ## Why `--worktree` is not a configurable option here
//!
//! Items in one level may point at the same clone — that is the monorepo
//! case, and it is the case the levelled plan exists to serve. Two concurrent
//! runs against one clone share one HEAD and one index, and the failure is
//! not a crash: `checkout -b` moves the shared HEAD, `add -A` stages the
//! other run's in-flight edits, and the surviving run then **reports a branch
//! that does not hold its commit**. That is false data fed straight into the
//! supervisor's accept-or-re-dispatch decision, and no amount of care further
//! up can detect it. So `--worktree` is passed unconditionally rather than
//! being exposed as a flag someone can forget: the cost of an unnecessary
//! checkout is a few seconds, the cost of omitting it is a wrong answer. See
//! `ORCHESTRATION.md` §4 and `SPEC.md`, "Amendment: worktree isolation and
//! the run summary".
//!
//! ## Why a non-zero exit is data
//!
//! Exit 2 (budget) and exit 3 (iteration ceiling) mean the run stopped early
//! **with its work committed**. Treating a non-zero status as a dispatch
//! failure would discard that commit and pay for it again. Only two things
//! are errors here: the process would not start, and stdout did not parse.
//! Everything else is handed to `disposition()` unchanged.
//!
//! ## Why there is no timeout
//!
//! A `codemason` run is already bounded from the inside — by its token
//! budget, by its iteration ceiling, and by the per-command timeout in
//! `tools::exec`. Adding a second, outer deadline here would mean killing a
//! process that is about to commit, which is the one outcome worth least.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;

use super::{Disposition, RunOutcome, WorkItem, disposition};

/// Concurrent `codemason` processes per host.
///
/// Not a CPU count. Each process builds and holds the search index for its
/// repository in memory, so the binding constraint is RAM, and the figure
/// that has actually been exercised is four concurrent processes (SPEC.md,
/// "Milestone Validation", item 5). Four is therefore the default because it
/// is the number with evidence behind it, not because it is the largest that
/// would fit; raise it deliberately, against a measurement on the target
/// repository.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// Matches `codemason run`'s own `--budget-tokens` default.
pub const DEFAULT_BUDGET_TOKENS: u64 = 200_000;

/// Matches `codemason run`'s own `--max-iterations` default, and the figure
/// written into the dispatch line in `ORCHESTRATION.md` §4.
pub const DEFAULT_MAX_ITERATIONS: u32 = 40;

/// Stderr kept in an error message, in bytes. The **tail** is kept: a
/// process that failed says why last.
const STDERR_CAP_BYTES: usize = 8 * 1024;

/// The only two things that count as a dispatch failure.
///
/// Everything a run can do to itself — exhausting its budget, hitting the
/// iteration ceiling, being gated, failing outright — arrives as a
/// `RunOutcome` instead, because all of those are decisions the supervisor
/// has to make rather than accidents it has to handle.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("item `{item_id}`: could not start `{binary}`: {source}")]
    Spawn {
        item_id: String,
        binary: String,
        #[source]
        source: std::io::Error,
    },

    #[error("item `{item_id}`: could not read the output of `{binary}`: {source}")]
    Output {
        item_id: String,
        binary: String,
        #[source]
        source: std::io::Error,
    },

    /// stdout was not one JSON object. Deliberately fatal for the item: a
    /// half-readable report is indistinguishable from a report about
    /// different work, and guessing at it would put invented facts into the
    /// supervisor's memory. stderr is carried along because it is the only
    /// place a human can find out what the process was complaining about.
    #[error(
        "item `{item_id}`: stdout was not a single JSON run report ({source})\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    )]
    UnreadableReport {
        item_id: String,
        #[source]
        source: serde_json::Error,
        stdout: String,
        stderr: String,
    },

    /// A worker thread unwound. Reported rather than swallowed so a level
    /// never silently comes back short of items.
    #[error("item `{item_id}`: the dispatch thread panicked")]
    ThreadPanicked { item_id: String },
}

/// How the level's items are dispatched. Everything here is per-dispatch
/// rather than per-item: items differ in what work they describe, not in how
/// the executor is run.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// The `codemason` executable. Plain `codemason` resolves through PATH,
    /// which is what a deployed supervisor wants; an absolute path is what a
    /// test wants.
    pub binary: PathBuf,
    /// Arguments inserted before `run`. Empty in production. It exists so a
    /// test can point `binary` at a shell and pass a script path, which is
    /// how the tests below avoid needing the real binary.
    pub binary_prefix_args: Vec<String>,
    /// `None` leaves model selection to `codemason`'s own resolution order,
    /// rather than duplicating that order here and letting the two drift.
    pub model: Option<String>,
    pub models_config: Option<PathBuf>,
    pub budget_tokens: u64,
    pub max_iterations: u32,
    pub keep_recent_turns: u32,
    /// See `DEFAULT_MAX_CONCURRENT`. Zero is treated as one.
    pub max_concurrent: usize,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("codemason"),
            binary_prefix_args: Vec::new(),
            model: None,
            models_config: None,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            keep_recent_turns: crate::compact::DEFAULT_KEEP_RECENT_TURNS,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }
}

/// One item's dispatch, finished.
///
/// The item id travels with the result because the caller has to be able to
/// attribute an outcome without relying on the ordering of a `Vec`, and
/// because `RunOutcome` carries `codemason`'s own run id, which is a
/// different identifier for a different thing.
#[derive(Debug)]
pub struct Execution {
    pub item_id: String,
    pub result: Result<RunOutcome, ExecError>,
    /// Wall clock for this item, measured around the child process. Not the
    /// run's self-reported `duration_ms`; the difference between the two is
    /// process startup and index build, which is the cost that decides how
    /// wide a level should be.
    pub duration_ms: u128,
}

impl Execution {
    /// What to do next with this item, or `None` when the dispatch itself
    /// failed and there is no exit code to decide from — that case is always
    /// a human's, since a process that would not start will not start on a
    /// retry either.
    pub fn disposition(&self) -> Option<Disposition> {
        self.result.as_ref().ok().map(|o| disposition(o.exit_code))
    }

    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.result.as_ref().ok()
    }
}

/// A counting gate. `std::sync` has no semaphore, and the alternative — a
/// pool of worker threads pulling from a queue — would delay process
/// creation behind whichever thread happened to pick the item up.
struct Gate {
    free: Mutex<usize>,
    released: Condvar,
}

impl Gate {
    fn new(permits: usize) -> Self {
        Self {
            free: Mutex::new(permits.max(1)),
            released: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.released.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
    }

    fn release(&self) {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        self.released.notify_one();
    }
}

/// The command line for one item.
///
/// Split out from the dispatch so the flags can be asserted on directly —
/// `--worktree` going missing is the kind of regression that produces
/// plausible-looking wrong answers rather than a test failure.
pub fn dispatch_args(item: &WorkItem, config: &DispatchConfig) -> Vec<String> {
    let mut args = config.binary_prefix_args.clone();
    args.push("run".to_string());
    args.push("--repo".to_string());
    args.push(item.repo.clone());
    args.push("--task".to_string());
    args.push(item.task.clone());
    // Non-negotiable; see the module header.
    args.push("--worktree".to_string());
    args.push("--budget-tokens".to_string());
    args.push(config.budget_tokens.to_string());
    args.push("--max-iterations".to_string());
    args.push(config.max_iterations.to_string());
    args.push("--keep-recent-turns".to_string());
    args.push(config.keep_recent_turns.to_string());
    if let Some(model) = &config.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if let Some(path) = &config.models_config {
        args.push("--models-config".to_string());
        args.push(path.display().to_string());
    }
    args
}

fn tail(bytes: &[u8], cap: usize) -> String {
    let start = bytes.len().saturating_sub(cap);
    let text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if start > 0 {
        format!("... truncated to the last {cap} bytes ...\n{text}")
    } else {
        text
    }
}

/// Turn one finished process into an outcome.
///
/// Pure, and separate from spawning, so the parsing half is testable on a
/// fixed byte string with no process involved.
///
/// `process_exit_code` is what the OS reported. Where it disagrees with the
/// `exit_code` field inside the report, the OS wins: the field is a
/// convenience copy, whereas the status is the contract the exit-code table
/// in `ORCHESTRATION.md` is written against, and a report truncated by a
/// crash would otherwise read as exit 0.
pub fn read_report(
    item_id: &str,
    process_exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<RunOutcome, ExecError> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    match serde_json::from_str::<RunOutcome>(trimmed) {
        Ok(mut outcome) => {
            if outcome.exit_code != process_exit_code {
                outcome.exit_code = process_exit_code;
            }
            Ok(outcome)
        }
        Err(source) => Err(ExecError::UnreadableReport {
            item_id: item_id.to_string(),
            source,
            stdout: tail(stdout, STDERR_CAP_BYTES),
            stderr: tail(stderr, STDERR_CAP_BYTES),
        }),
    }
}

/// Run one item to completion. Blocking; `dispatch_level` is what gives
/// concurrency.
pub fn dispatch_item(item: &WorkItem, config: &DispatchConfig) -> Result<RunOutcome, ExecError> {
    let binary = config.binary.display().to_string();

    let mut command = Command::new(&config.binary);
    command
        .args(dispatch_args(item, config))
        // stdin is closed rather than inherited: several concurrent children
        // sharing one console stdin would compete for it, and `codemason`
        // never reads from it.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn().map_err(|source| ExecError::Spawn {
        item_id: item.id.clone(),
        binary: binary.clone(),
        source,
    })?;

    // `wait_with_output` drains both pipes while the child runs. Waiting
    // first and reading after would deadlock the moment a run wrote more
    // than a pipe buffer of diagnostics to stderr, which a verbose run does.
    let output = child
        .wait_with_output()
        .map_err(|source| ExecError::Output {
            item_id: item.id.clone(),
            binary,
            source,
        })?;

    // `None` means killed by a signal; -1 is outside the documented table and
    // so falls through `disposition()` to `Escalate`, which is correct for a
    // run that was shot.
    let code = output.status.code().unwrap_or(-1);
    read_report(&item.id, code, &output.stdout, &output.stderr)
}

/// Dispatch every item in one level concurrently and wait for all of them.
///
/// Every item gets its own thread up front, and each thread spawns its child
/// process as its first action — so the processes are all started before any
/// of them is waited on, which is the entire point of a level. The only
/// thing that holds a thread back is the concurrency gate.
///
/// This is the hard gate between levels: it returns only when the last item
/// has finished. Results come back in the order the items were given,
/// regardless of the order they completed in.
pub fn dispatch_level(items: &[WorkItem], config: &DispatchConfig) -> Vec<Execution> {
    if items.is_empty() {
        return Vec::new();
    }

    let config = Arc::new(config.clone());
    let gate = Arc::new(Gate::new(config.max_concurrent));

    let mut handles = Vec::with_capacity(items.len());
    for item in items {
        let item = item.clone();
        let config = Arc::clone(&config);
        let gate = Arc::clone(&gate);
        let id = item.id.clone();
        handles.push((
            id,
            thread::spawn(move || {
                gate.acquire();
                let started = Instant::now();
                let result = dispatch_item(&item, &config);
                gate.release();
                (result, started.elapsed().as_millis())
            }),
        ));
    }

    handles
        .into_iter()
        .map(|(item_id, handle)| match handle.join() {
            Ok((result, duration_ms)) => Execution {
                item_id,
                result,
                duration_ms,
            },
            Err(_) => Execution {
                item_id: item_id.clone(),
                result: Err(ExecError::ThreadPanicked { item_id }),
                duration_ms: 0,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn item(id: &str, repo: &str) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            partition_id: None,
            level: 1,
            repo: repo.to_string(),
            task: "do the thing".to_string(),
            acceptance: None,
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codemason-execute-test-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in for `codemason run`: sleeps, writes a canned stdout and
    /// stderr, exits with a chosen code. Returns the `binary` and
    /// `binary_prefix_args` a `DispatchConfig` needs to invoke it, so no test
    /// here needs the real binary, a repository, or a network.
    #[cfg(windows)]
    fn fake_binary(
        dir: &Path,
        label: &str,
        sleep_secs: u32,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
    ) -> (PathBuf, Vec<String>) {
        // `ping` rather than `timeout`: a coreutils `timeout` earlier on PATH
        // than System32's would silently change what this measures, and
        // `timeout` also refuses to run without a console. `ping -n N` waits
        // N-1 seconds.
        let wait = if sleep_secs > 0 {
            format!("ping -n {} 127.0.0.1 >nul\r\n", sleep_secs + 1)
        } else {
            String::new()
        };
        let stderr_line = if stderr.is_empty() {
            String::new()
        } else {
            format!("echo {stderr} 1>&2\r\n")
        };
        let stdout_line = if stdout.is_empty() {
            String::new()
        } else {
            format!("echo {stdout}\r\n")
        };
        let script =
            format!("@echo off\r\n{wait}{stderr_line}{stdout_line}exit /b {exit_code}\r\n");
        let path = dir.join(format!("{label}.cmd"));
        std::fs::write(&path, script).unwrap();
        // Invoked through `cmd /C <path>` rather than directly: a batch file
        // is not an executable image, and going through the shell explicitly
        // keeps this independent of how the standard library chooses to
        // handle `.cmd` today.
        (
            PathBuf::from("cmd"),
            vec!["/C".to_string(), path.display().to_string()],
        )
    }

    #[cfg(not(windows))]
    fn fake_binary(
        dir: &Path,
        label: &str,
        sleep_secs: u32,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
    ) -> (PathBuf, Vec<String>) {
        let wait = if sleep_secs > 0 {
            format!("sleep {sleep_secs}\n")
        } else {
            String::new()
        };
        let stderr_line = if stderr.is_empty() {
            String::new()
        } else {
            format!("echo '{stderr}' 1>&2\n")
        };
        let stdout_line = if stdout.is_empty() {
            String::new()
        } else {
            format!("echo '{stdout}'\n")
        };
        let script = format!("{wait}{stderr_line}{stdout_line}exit {exit_code}\n");
        let path = dir.join(format!("{label}.sh"));
        std::fs::write(&path, script).unwrap();
        // `sh <path>` avoids needing to set an executable bit.
        (PathBuf::from("/bin/sh"), vec![path.display().to_string()])
    }

    fn config_for(binary: PathBuf, prefix: Vec<String>) -> DispatchConfig {
        DispatchConfig {
            binary,
            binary_prefix_args: prefix,
            ..Default::default()
        }
    }

    #[test]
    fn the_dispatch_always_passes_worktree() {
        // Concurrent runs that share a clone corrupt each other's git state
        // and misreport the branch. If this line ever goes, the supervisor
        // starts believing things that are not true.
        let args = dispatch_args(&item("a", "/repo"), &DispatchConfig::default());
        assert!(args.contains(&"--worktree".to_string()), "{args:?}");
        assert_eq!(args.first().map(String::as_str), Some("run"));
        assert!(args.contains(&"--budget-tokens".to_string()));
        assert!(args.contains(&"--max-iterations".to_string()));
        assert!(args.contains(&"--keep-recent-turns".to_string()));
    }

    #[test]
    fn configured_model_and_models_config_reach_the_command_line() {
        let config = DispatchConfig {
            model: Some("gpt-4o-mini".to_string()),
            models_config: Some(PathBuf::from("models.toml")),
            budget_tokens: 1234,
            ..Default::default()
        };
        let args = dispatch_args(&item("a", "/repo"), &config);
        let joined = args.join(" ");
        assert!(joined.contains("--model gpt-4o-mini"), "{joined}");
        assert!(joined.contains("--models-config models.toml"), "{joined}");
        assert!(joined.contains("--budget-tokens 1234"), "{joined}");
    }

    #[test]
    fn a_well_formed_report_parses() {
        let stdout = br#"{"run_id":"r1","status":"completed","exit_code":0,
                          "branch":"codemason/x","commit":"abc123",
                          "files_changed":["src/a.rs"],"iterations":7,
                          "totals":{"total_tokens":900,"cost":0.02}}"#;
        let outcome = read_report("a", 0, stdout, b"").expect("well-formed report");
        assert_eq!(outcome.run_id, "r1");
        assert_eq!(outcome.commit.as_deref(), Some("abc123"));
        assert_eq!(outcome.files_changed, vec!["src/a.rs".to_string()]);
        assert_eq!(outcome.totals.total_tokens, 900);
        assert_eq!(disposition(outcome.exit_code), Disposition::Verify);
    }

    #[test]
    fn unparseable_stdout_is_an_error_carrying_stderr() {
        let err = read_report("a", 1, b"Traceback: not json", b"panicked at src/x.rs")
            .expect_err("garbage stdout must not be guessed at");
        let text = err.to_string();
        assert!(text.contains("panicked at src/x.rs"), "{text}");
        assert!(text.contains("item `a`"), "{text}");
    }

    #[test]
    fn the_process_status_wins_over_the_reported_field() {
        // A report truncated by a crash would otherwise read as exit 0 and be
        // accepted as a completed run.
        let outcome = read_report("a", 3, br#"{"run_id":"r","exit_code":0}"#, b"").unwrap();
        assert_eq!(outcome.exit_code, 3);
    }

    #[test]
    fn a_failure_to_spawn_is_an_error_not_an_outcome() {
        let config = config_for(
            PathBuf::from("codemason-no-such-binary-exists-here"),
            Vec::new(),
        );
        let err = dispatch_item(&item("a", "."), &config).expect_err("must not start");
        assert!(matches!(err, ExecError::Spawn { .. }), "{err}");
    }

    #[test]
    fn exit_two_with_a_commit_is_not_an_error_and_verifies() {
        // The row a naive supervisor gets wrong: budget exhausted, work
        // committed. Discarding it pays for the same commit twice.
        let dir = temp_dir("exit-two");
        let (binary, prefix) = fake_binary(
            &dir,
            "budget",
            0,
            r#"{"run_id":"r2","status":"budget_exceeded","exit_code":2,"commit":"deadbee","branch":"codemason/b"}"#,
            "budget exceeded before iteration 9",
            2,
        );
        let config = config_for(binary, prefix);

        let results = dispatch_level(&[item("a", ".")], &config);
        assert_eq!(results.len(), 1);
        let outcome = results[0]
            .result
            .as_ref()
            .expect("a non-zero exit is data, not a dispatch failure");
        assert_eq!(outcome.exit_code, 2);
        assert_eq!(outcome.commit.as_deref(), Some("deadbee"));
        assert_eq!(results[0].disposition(), Some(Disposition::Verify));
    }

    #[test]
    fn unparseable_stdout_from_a_real_process_carries_its_stderr() {
        let dir = temp_dir("garbage");
        let (binary, prefix) = fake_binary(
            &dir,
            "garbage",
            0,
            "this is not json",
            "config error: no such model",
            1,
        );
        let config = config_for(binary, prefix);

        let results = dispatch_level(&[item("a", ".")], &config);
        let err = results[0]
            .result
            .as_ref()
            .expect_err("unreadable stdout is a hard error for the item");
        assert!(matches!(err, ExecError::UnreadableReport { .. }), "{err}");
        assert!(err.to_string().contains("no such model"), "{err}");
        assert_eq!(results[0].disposition(), None);
    }

    #[test]
    fn a_level_runs_its_items_concurrently_not_one_after_another() {
        // The premise of the whole stage. Three items that each take about
        // three seconds must finish in far less than nine.
        let dir = temp_dir("concurrent");
        let (binary, prefix) = fake_binary(
            &dir,
            "slow",
            3,
            r#"{"run_id":"r","status":"completed","exit_code":0}"#,
            "",
            0,
        );
        let config = config_for(binary, prefix);

        let items = vec![item("a", "."), item("b", "."), item("c", ".")];
        let started = Instant::now();
        let results = dispatch_level(&items, &config);
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.result.is_ok(), "{:?}", r.result);
        }
        // Serial would be ~9s. A generous margin: this asserts the shape of
        // the schedule, not the speed of the machine.
        assert!(
            elapsed < Duration::from_secs(7),
            "three 3s items took {elapsed:?}, which looks serial"
        );
    }

    #[test]
    fn results_come_back_in_item_order_whatever_order_they_finish_in() {
        let dir = temp_dir("ordering");
        let (binary, prefix) = fake_binary(
            &dir,
            "quick",
            0,
            r#"{"run_id":"r","status":"completed","exit_code":0}"#,
            "",
            0,
        );
        let config = config_for(binary, prefix);

        let items = vec![item("first", "."), item("second", "."), item("third", ".")];
        let ids: Vec<String> = dispatch_level(&items, &config)
            .into_iter()
            .map(|e| e.item_id)
            .collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }

    #[test]
    fn the_concurrency_cap_bounds_processes_in_flight() {
        // Each process holds an index in memory, so the cap is what keeps a
        // wide level from exhausting the host. With a cap of one, three
        // three-second items must take at least six seconds.
        let dir = temp_dir("capped");
        let (binary, prefix) = fake_binary(
            &dir,
            "capped",
            3,
            r#"{"run_id":"r","status":"completed","exit_code":0}"#,
            "",
            0,
        );
        let config = DispatchConfig {
            max_concurrent: 1,
            ..config_for(binary, prefix)
        };

        let items = vec![item("a", "."), item("b", "."), item("c", ".")];
        let started = Instant::now();
        let results = dispatch_level(&items, &config);
        let elapsed = started.elapsed();

        assert_eq!(results.len(), 3);
        assert!(
            elapsed >= Duration::from_secs(6),
            "cap of 1 finished in {elapsed:?}, so it did not serialise"
        );
    }

    #[test]
    fn an_empty_level_does_nothing() {
        assert!(dispatch_level(&[], &DispatchConfig::default()).is_empty());
    }
}
