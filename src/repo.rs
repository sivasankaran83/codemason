//! Every repository operation this binary performs against the *target*
//! repository shells out to the `git` CLI — never a git library — so this is
//! the one place that would need to change to substitute worktree isolation
//! later without touching the loop.
//!
//! `.agent/` (this tool's own log directory, already excluded from
//! `list_files`) is excluded from both the preflight clean-check and the
//! postflight `git add` via pathspec magic, so a prior run's leftover log
//! never blocks the next run and is never swept into a commit.

use std::path::Path;
use std::process::Command;

use crate::error::Error;

pub struct CommitInfo {
    pub sha: String,
    pub files_changed: Vec<String>,
}

const EXCLUDE_AGENT: &str = ":!.agent";

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, Error> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|source| Error::GitCommand {
            args: args.join(" "),
            source: source.into(),
        })?;

    if !output.status.success() {
        return Err(Error::GitCommand {
            args: args.join(" "),
            source: anyhow::anyhow!("{}", String::from_utf8_lossy(&output.stderr).trim()),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(repo_root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Confirm `repo_root` is a git working tree and clean, unless `dry_run` is
/// set — nothing is checked and nothing is refused when the run is
/// simulated. Runs before any network call so a dirty tree is caught before
/// anything is spent.
pub fn preflight(repo_root: &Path, dry_run: bool) -> Result<(), Error> {
    if dry_run {
        return Ok(());
    }

    if !git_ok(repo_root, &["rev-parse", "--is-inside-work-tree"]) {
        return Err(Error::NotAGitWorktree {
            path: repo_root.to_path_buf(),
        });
    }

    let status = run_git(
        repo_root,
        &["status", "--porcelain", "--", ".", EXCLUDE_AGENT],
    )?;
    if !status.is_empty() {
        return Err(Error::DirtyWorktree {
            path: repo_root.to_path_buf(),
        });
    }

    Ok(())
}

/// Create and check out a new branch. Never called under `--dry-run`.
pub fn create_branch(repo_root: &Path, name: &str) -> Result<(), Error> {
    run_git(repo_root, &["checkout", "-b", name]).map(|_| ())
}

/// Stage everything except `.agent/`, and commit only if that leaves
/// something staged. Returns `None` when nothing changed — a run that
/// touched nothing produces no commit.
pub fn commit_all(repo_root: &Path, message: &str) -> Result<Option<CommitInfo>, Error> {
    run_git(repo_root, &["add", "-A", "--", ".", EXCLUDE_AGENT])?;

    let staged = run_git(repo_root, &["diff", "--cached", "--name-only"])?;
    let files_changed: Vec<String> = staged
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if files_changed.is_empty() {
        return Ok(None);
    }

    run_git(repo_root, &["commit", "-m", message])?;
    let sha = run_git(repo_root, &["rev-parse", "HEAD"])?;

    Ok(Some(CommitInfo { sha, files_changed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codemason-repo-test-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_git_repo(dir: &Path) {
        assert!(git_ok(dir, &["init", "-q"]));
        assert!(git_ok(dir, &["config", "user.email", "test@example.com"]));
        assert!(git_ok(dir, &["config", "user.name", "test"]));
    }

    #[test]
    fn preflight_rejects_a_non_git_directory() {
        let dir = temp_repo("not-git");
        let err = preflight(&dir, false).expect_err("should reject a non-git directory");
        assert!(matches!(err, Error::NotAGitWorktree { .. }));
    }

    #[test]
    fn preflight_dry_run_skips_every_check() {
        let dir = temp_repo("dry-run-skip");
        // Not even a git repo, yet dry_run must short-circuit to Ok.
        preflight(&dir, true).expect("dry_run must skip the check entirely");
    }

    #[test]
    fn preflight_rejects_a_dirty_tree_and_accepts_a_clean_one() {
        let dir = temp_repo("dirty");
        init_git_repo(&dir);
        fs::write(dir.join("a.txt"), "hello").unwrap();
        assert!(git_ok(&dir, &["add", "-A"]));
        assert!(git_ok(&dir, &["commit", "-q", "-m", "initial"]));

        preflight(&dir, false).expect("a freshly committed tree should be clean");

        fs::write(dir.join("a.txt"), "hello, dirty").unwrap();
        let err = preflight(&dir, false).expect_err("an uncommitted change should be dirty");
        assert!(matches!(err, Error::DirtyWorktree { .. }));
    }

    #[test]
    fn preflight_ignores_a_leftover_agent_directory() {
        let dir = temp_repo("agent-dir");
        init_git_repo(&dir);
        fs::write(dir.join("a.txt"), "hello").unwrap();
        assert!(git_ok(&dir, &["add", "-A"]));
        assert!(git_ok(&dir, &["commit", "-q", "-m", "initial"]));

        fs::create_dir_all(dir.join(".agent/log")).unwrap();
        fs::write(dir.join(".agent/log/run-x.jsonl"), "{}").unwrap();

        preflight(&dir, false)
            .expect("an untracked .agent/ directory must not count as dirty");
    }

    #[test]
    fn commit_all_returns_none_when_nothing_changed() {
        let dir = temp_repo("no-op-commit");
        init_git_repo(&dir);
        fs::write(dir.join("a.txt"), "hello").unwrap();
        assert!(git_ok(&dir, &["add", "-A"]));
        assert!(git_ok(&dir, &["commit", "-q", "-m", "initial"]));

        let result = commit_all(&dir, "no-op").expect("commit_all should not error");
        assert!(result.is_none());
    }

    #[test]
    fn commit_all_commits_changes_and_excludes_agent() {
        let dir = temp_repo("real-commit");
        init_git_repo(&dir);
        fs::write(dir.join("a.txt"), "hello").unwrap();
        assert!(git_ok(&dir, &["add", "-A"]));
        assert!(git_ok(&dir, &["commit", "-q", "-m", "initial"]));

        fs::write(dir.join("a.txt"), "hello, changed").unwrap();
        fs::create_dir_all(dir.join(".agent/log")).unwrap();
        fs::write(dir.join(".agent/log/run-x.jsonl"), "{}").unwrap();

        let info = commit_all(&dir, "codemason: test change")
            .expect("commit_all should not error")
            .expect("a real change should produce a commit");

        assert_eq!(info.files_changed, vec!["a.txt".to_string()]);
        assert!(!info.sha.is_empty());

        let tracked = run_git(&dir, &["ls-files"]).unwrap();
        assert!(!tracked.contains(".agent"));
    }

    #[test]
    fn create_branch_checks_out_a_new_branch() {
        let dir = temp_repo("branch");
        init_git_repo(&dir);
        fs::write(dir.join("a.txt"), "hello").unwrap();
        assert!(git_ok(&dir, &["add", "-A"]));
        assert!(git_ok(&dir, &["commit", "-q", "-m", "initial"]));

        create_branch(&dir, "codemason/test-branch").expect("branch creation should succeed");
        let current = run_git(&dir, &["branch", "--show-current"]).unwrap();
        assert_eq!(current, "codemason/test-branch");
    }
}
