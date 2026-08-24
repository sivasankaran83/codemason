//! Every repository operation this binary performs against the *target*
//! repository shells out to the `git` CLI — never a git library — so this is
//! the one place that changes when repository isolation changes. Worktree
//! isolation was the substitution this module was shaped for, and it lives
//! here: the loop and the stdout contract are untouched by it.
//!
//! `.agent/` (this tool's own log directory, already excluded from
//! `list_files`) is excluded from both the preflight clean-check and the
//! postflight `git add` via pathspec magic, so a prior run's leftover log
//! never blocks the next run and is never swept into a commit.
//!
//! ## Why worktree isolation exists
//!
//! Two concurrent runs against *separate clones* are safe and always were.
//! Two concurrent runs against *the same clone* — the natural way to work on
//! two sections of one monorepo — are not, and fail in three ways at once,
//! all of them from git state that is per-repository rather than per-run:
//! `checkout -b` moves the one shared HEAD, `add -A` stages the other run's
//! in-flight edits, and the losing run dies on a ref lock. Worse than any of
//! those, the surviving run reports a branch that does not hold its commit —
//! the stdout contract goes from narrow to wrong, which is the one thing a
//! supervisor above it cannot defend against.
//!
//! A worktree gives each run its own HEAD, its own index and its own working
//! directory while sharing one object store, which removes the shared state
//! rather than trying to schedule around it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;

pub struct CommitInfo {
    pub sha: String,
    pub files_changed: Vec<String>,
}

/// An isolated working tree created for one run, plus where the run should
/// actually operate inside it.
pub struct Worktree {
    /// Root of the isolated working tree — what `git worktree remove` takes.
    pub root: PathBuf,
    /// The path the run works against. Equal to `root` when `--repo` pointed
    /// at a repository root, or `root` joined with the section subpath when
    /// `--repo` pointed at a subdirectory of a monorepo.
    pub work_path: PathBuf,
    /// Top level of the original repository — every `git worktree` command
    /// has to run from there, not from inside the worktree.
    origin_top: PathBuf,
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

/// Create and check out a new branch. Never called under `--dry-run`, and
/// never called at all when the run is worktree-isolated — `worktree_add`
/// creates the branch as part of creating the tree.
pub fn create_branch(repo_root: &Path, name: &str) -> Result<(), Error> {
    run_git(repo_root, &["checkout", "-b", name]).map(|_| ())
}

/// Top level of the repository containing `path`, canonicalised.
fn toplevel(path: &Path) -> Result<PathBuf, Error> {
    let raw = run_git(path, &["rev-parse", "--show-toplevel"])?;
    std::fs::canonicalize(&raw).map_err(|source| Error::GitCommand {
        args: format!("rev-parse --show-toplevel ({raw})"),
        source: source.into(),
    })
}

/// Create an isolated worktree for one run, on a new branch, checked out at
/// the current `HEAD` of the repository containing `repo_path`.
///
/// `repo_path` may be a repository root or any subdirectory of one. When it
/// is a subdirectory — one service of a monorepo — the same relative section
/// is resolved inside the worktree and returned as `work_path`, so the run
/// sees exactly the section it was pointed at and nothing above it.
///
/// The branch outlives the worktree: removing the tree leaves the commits
/// reachable from the branch, which is what the supervisor above merges.
pub fn worktree_add(repo_path: &Path, branch: &str, at: &Path) -> Result<Worktree, Error> {
    let origin_top = toplevel(repo_path)?;
    let abs_repo = std::fs::canonicalize(repo_path).map_err(|source| Error::GitCommand {
        args: format!("canonicalize {}", repo_path.display()),
        source: source.into(),
    })?;

    // Empty when --repo already pointed at the repository root.
    let section = abs_repo.strip_prefix(&origin_top).unwrap_or(Path::new(""));

    let at_str = at.to_string_lossy().into_owned();
    run_git(
        &origin_top,
        &["worktree", "add", "-b", branch, &at_str, "HEAD"],
    )?;

    // `git worktree add` created it, so it exists and canonicalises.
    let root = std::fs::canonicalize(at).map_err(|source| Error::GitCommand {
        args: format!("canonicalize {}", at.display()),
        source: source.into(),
    })?;
    let work_path = if section.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(section)
    };

    Ok(Worktree {
        root,
        work_path,
        origin_top,
    })
}

/// Remove an isolated worktree, leaving its branch behind.
///
/// `--force` because the run is expected to leave untracked files in the
/// tree (its own `.agent/` directory at minimum), and a worktree that
/// refuses to be removed would leak a directory per run.
pub fn worktree_remove(worktree: &Worktree) -> Result<(), Error> {
    let root = worktree.root.to_string_lossy().into_owned();
    run_git(
        &worktree.origin_top,
        &["worktree", "remove", "--force", &root],
    )?;
    // Best-effort: drop any administrative leftovers so a later `worktree
    // list` stays honest. Never fatal — the tree itself is already gone.
    let _ = run_git(&worktree.origin_top, &["worktree", "prune"]);
    Ok(())
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
