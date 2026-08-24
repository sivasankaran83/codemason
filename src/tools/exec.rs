//! `run_command`. Executes at the repository root via the platform shell.
//! No command allowlist — the isolation boundary is the container and the
//! disposable repository copy, documented in README.md's Safety section, not
//! a filter enforced here.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::tools::{ToolContext, ToolOutcome};

const DEFAULT_TIMEOUT_SECS: i64 = 120;
const MAX_TIMEOUT_SECS: i64 = 900;
const OUTPUT_CAP_BYTES: usize = 100 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Interleaved stdout+stderr, capped to the **last** `OUTPUT_CAP_BYTES` —
/// build errors appear at the end, so the tail is what's worth keeping.
struct CapturedOutput {
    buffer: Mutex<Vec<u8>>,
    truncated: Mutex<bool>,
}

impl CapturedOutput {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            buffer: Mutex::new(Vec::new()),
            truncated: Mutex::new(false),
        })
    }

    fn push(&self, chunk: &[u8]) {
        let mut buf = self.buffer.lock().unwrap();
        buf.extend_from_slice(chunk);
        if buf.len() > OUTPUT_CAP_BYTES {
            let excess = buf.len() - OUTPUT_CAP_BYTES;
            buf.drain(0..excess);
            *self.truncated.lock().unwrap() = true;
        }
    }

    fn finish(&self) -> (String, bool) {
        let buf = self.buffer.lock().unwrap();
        (
            String::from_utf8_lossy(&buf).into_owned(),
            *self.truncated.lock().unwrap(),
        )
    }
}

fn spawn_reader(
    mut stream: impl Read + Send + 'static,
    sink: Arc<CapturedOutput>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.push(&chunk[..n]),
            }
        }
    })
}

fn clamp_timeout(requested: i64) -> i64 {
    if requested <= 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        requested.min(MAX_TIMEOUT_SECS)
    }
}

#[cfg(windows)]
fn platform_command(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(windows))]
fn platform_command(command: &str) -> Command {
    // `setsid` puts the invoked shell in a new session/process group whose
    // PGID equals its own PID, so the whole tree can be killed by signaling
    // the negative PGID below without any unsafe pre_exec code.
    let mut cmd = Command::new("setsid");
    cmd.args(["sh", "-c", command]);
    cmd
}

#[cfg(windows)]
fn kill_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    let pgid = format!("-{pid}");
    let _ = Command::new("kill").args(["-TERM", &pgid]).output();
    thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill").args(["-KILL", &pgid]).output();
}

/// Run `command` at the repository root. A non-zero exit is a normal
/// `ToolOutcome::Ok` result, not an error — only a failure to start the
/// process, or a timeout, produces `ToolOutcome::Error`.
pub fn run_command(ctx: &ToolContext, command: &str, timeout_seconds: i64) -> ToolOutcome {
    if ctx.dry_run {
        return ToolOutcome::Error(
            "run_command: --dry-run is set, the command was not executed".to_string(),
        );
    }
    if command.trim().is_empty() {
        return ToolOutcome::Error("command must not be empty".to_string());
    }

    let timeout = clamp_timeout(timeout_seconds);

    let mut cmd = platform_command(command);
    cmd.current_dir(ctx.repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => return ToolOutcome::Error(format!("failed to start command: {err}")),
    };

    let output = CapturedOutput::new();
    let mut handles = Vec::new();
    if let Some(s) = child.stdout.take() {
        handles.push(spawn_reader(s, output.clone()));
    }
    if let Some(s) = child.stderr.take() {
        handles.push(spawn_reader(s, output.clone()));
    }

    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(timeout as u64);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break None,
        }
    };

    match status {
        Some(exit_status) => {
            for h in handles {
                let _ = h.join();
            }
            let (text, truncated) = output.finish();
            let code = exit_status.code().unwrap_or(-1);
            let mut out = format!("exit code: {code}\n");
            if truncated {
                out.push_str(&format!(
                    "... output truncated to the last {OUTPUT_CAP_BYTES} bytes ...\n"
                ));
            }
            out.push_str(&text);
            ToolOutcome::Ok(out)
        }
        None => {
            kill_tree(pid);
            let _ = child.wait();
            for h in handles {
                let _ = h.join();
            }
            let (text, truncated) = output.finish();
            let mut out = format!("command timed out after {timeout}s and was terminated\n");
            if truncated {
                out.push_str(&format!(
                    "... output truncated to the last {OUTPUT_CAP_BYTES} bytes ...\n"
                ));
            }
            out.push_str(&text);
            ToolOutcome::Error(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codemason-exec-tool-test-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn index_for(dir: &std::path::Path) -> Index {
        std::fs::write(dir.join("seed.rs"), "fn main() {}\n").unwrap();
        Index::build(dir).expect("index build should succeed")
    }

    #[test]
    fn dry_run_refuses_without_executing() {
        let dir = temp_repo("dry-run");
        let index = index_for(&dir);
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: true,
        };

        let outcome = run_command(&ctx, "echo hi", 0);
        assert!(matches!(outcome, ToolOutcome::Error(_)));
    }

    #[test]
    #[cfg(windows)]
    fn non_zero_exit_is_a_normal_result_not_an_error() {
        let dir = temp_repo("nonzero-exit");
        let index = index_for(&dir);
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        let outcome = run_command(&ctx, "exit 1", 30);
        match outcome {
            ToolOutcome::Ok(text) => assert!(text.contains("exit code: 1")),
            ToolOutcome::Error(err) => panic!("non-zero exit should be Ok, got Error: {err}"),
        }
    }

    #[test]
    #[cfg(windows)]
    fn output_over_the_cap_keeps_the_last_100kb_with_a_truncation_notice() {
        let dir = temp_repo("truncate");
        let index = index_for(&dir);
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        // A plain cmd.exe `for /L` loop prints well over the 100 KB cap —
        // no external tool, no nested quoting, nothing PATH-order-sensitive.
        let command =
            "for /L %i in (1,1,3000) do @echo 0123456789012345678901234567890123456789012345678901234567890123456789";
        let outcome = run_command(&ctx, command, 60);
        match outcome {
            ToolOutcome::Ok(text) => {
                assert!(text.contains("truncated to the last"));
                let body_len = text.rsplit("...\n").next().unwrap().len();
                assert!(body_len <= OUTPUT_CAP_BYTES);
            }
            ToolOutcome::Error(err) => panic!("unexpected error: {err}"),
        }
    }

    #[test]
    #[cfg(windows)]
    fn timeout_kills_the_full_process_tree_not_just_the_shell() {
        let dir = temp_repo("timeout-tree");
        let index = index_for(&dir);
        let ctx = ToolContext {
            repo_root: &dir,
            index: &index,
            dry_run: false,
        };

        // `start /B` detaches a grandchild `ping.exe` from the immediate
        // `cmd.exe` child, and the outer command itself also blocks in its
        // own `ping` well past the 1s timeout — if only the shell process
        // were killed (not its whole tree), both `ping.exe` processes would
        // outlive it. `ping` is used instead of `timeout` because a
        // coreutils `timeout` earlier on PATH than System32's would silently
        // change this test's meaning; `ping.exe` has no such conflict.
        let command = "start /B ping -n 31 127.0.0.1 >nul & ping -n 31 127.0.0.1 >nul";
        let outcome = run_command(&ctx, command, 1);
        assert!(matches!(outcome, ToolOutcome::Error(_)), "{outcome:?}");

        std::thread::sleep(Duration::from_millis(500));
        let tasklist = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&tasklist.stdout);
        assert!(
            !listing.contains("ping.exe"),
            "leftover process tree after kill: {listing}"
        );
    }
}
