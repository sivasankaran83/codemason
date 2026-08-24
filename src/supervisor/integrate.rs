//! Stage 5, INTEGRATE — merge a level's branches, run the acceptance command,
//! and decide whether to re-dispatch or escalate.
//!
//! This is the verifier in the AgentFlow loop, and on the evidence in
//! `ORCHESTRATION.md` it is the highest-value component in the design:
//! uncoordinated parallel agents amplify errors roughly 17x over a single
//! agent, a central integrator contains that to roughly 4x.
//!
//! Three positions are deliberate and are the reason this module is not
//! simply a shell script.
//!
//! **The acceptance command is the gate, and nothing else is.** A model's
//! `summary` is its own account of what it did — worth logging, worth showing
//! a human, never evidence. What actually happened is the merge result and
//! the exit status of the acceptance command.
//!
//! **Conflicts are recorded, never resolved.** Machine resolution is out of
//! scope, and the measured reason is not squeamishness: in the session that
//! shaped this design, textual conflicts did not occur at all once
//! partitioning was done properly, while every failure that *did* occur was
//! semantic and would have merged clean anyway. A merge agent would therefore
//! have spent money on the problem nobody had. A conflict here means the
//! partitioning was wrong, which is a planning defect and a human's to fix.
//!
//! **Fixing is only allowed to continue while the error set is shrinking.**
//! See [`is_converging`] — this is the part worth reading.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{Disposition, MAX_FIX_CYCLES, RunOutcome, WorkItem, disposition};

#[derive(Debug, thiserror::Error)]
pub enum IntegrateError {
    #[error("git {args} failed: {source}")]
    Git {
        args: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to start acceptance command `{command}`: {source}")]
    Acceptance {
        command: String,
        #[source]
        source: anyhow::Error,
    },
    /// An item with nothing to run cannot be gated, and quietly treating that
    /// as a pass is how unverified work reaches a base branch.
    #[error("no acceptance command: the level cannot be verified")]
    NoAcceptanceCommand,
}

// --- git, shelled out ------------------------------------------------------
//
// Same rule as `crate::repo`: every repository operation goes through the
// `git` CLI and no git library is linked. These helpers are local rather than
// reused from `repo` because that module's error type belongs to the
// executor's exit-code contract and this one does not.

/// stdout, stderr and success for one `git` invocation. Merge failure is an
/// expected outcome here rather than an error, so callers need the exit
/// status without an early return.
fn git(repo_root: &Path, args: &[&str]) -> Result<(bool, String, String), IntegrateError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| IntegrateError::Git {
            args: args.join(" "),
            source: source.into(),
        })?;

    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    ))
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String, IntegrateError> {
    let (ok, stdout, stderr) = git(repo_root, args)?;
    if !ok {
        return Err(IntegrateError::Git {
            args: args.join(" "),
            source: anyhow::anyhow!("{stderr}"),
        });
    }
    Ok(stdout)
}

/// Create or reset the integration branch at `base`, and check it out.
///
/// `-B` because the integration branch is derived state: it holds nothing
/// that is not reachable from `base` and the level's own branches, so
/// rebuilding it from scratch is the intended behaviour rather than a loss.
pub fn create_integration_branch(
    repo_root: &Path,
    name: &str,
    base: &str,
) -> Result<(), IntegrateError> {
    run_git(repo_root, &["checkout", "-q", "-B", name, base]).map(|_| ())
}

// --- what a level produced -------------------------------------------------

/// One finished work item: what was asked for, and what the process reported.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletedItem {
    pub item: WorkItem,
    pub outcome: RunOutcome,
}

/// Whether an item's branch is worth merging at all.
///
/// Exit codes 2 and 3 stopped early but **committed their work**, and the
/// acceptance command is the only thing entitled to judge whether that work
/// is complete — so they merge like any other. Escalating and gated runs have
/// nothing trustworthy on their branch.
pub fn mergeable(outcome: &RunOutcome) -> bool {
    disposition(outcome.exit_code) == Disposition::Verify
        && outcome.branch.as_deref().is_some_and(|b| !b.is_empty())
}

/// Branches to merge, in level order.
///
/// Level order matters even though items within a level are disjoint by
/// construction: a later level was planned against the earlier one's result,
/// so merging it first would present the tree it was not written for.
pub fn merge_order(completed: &[CompletedItem]) -> Vec<String> {
    let mut ordered: Vec<&CompletedItem> = completed.iter().collect();
    ordered.sort_by_key(|c| c.item.level);
    ordered
        .into_iter()
        .filter(|c| mergeable(&c.outcome))
        .filter_map(|c| c.outcome.branch.clone())
        .collect()
}

/// A branch that would not merge, and the paths git could not reconcile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub branch: String,
    pub files: Vec<String>,
}

// --- the acceptance gate ---------------------------------------------------

/// A code we could not name. The set still moves when the count does, so
/// convergence degrades to count-only comparison rather than breaking.
const GENERIC_ERROR_CODE: &str = "ERROR";

/// `error CS0246`, `error NU1101` — the MSBuild/NuGet shape, and the same
/// shape most compilers with a diagnostic catalogue emit.
static CODED_ERROR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\berror\s+([a-z]{2,10}[0-9]{2,6})\b").expect("valid regex"));

/// `error[E0308]` — the rustc shape.
static BRACKETED_ERROR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\berror\[([a-z]?[0-9]{2,6})\]").expect("valid regex"));

/// A line that is reporting an error. `\berror\b` does not match "errors",
/// so prose summaries mostly fall out on their own.
static ERROR_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\berror\b").expect("valid regex"));

/// `    3 Error(s)` — a tally of diagnostics already listed above it.
/// Counting it would inflate every MSBuild run by one.
static ERROR_TALLY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^\s*[0-9]+\s+error").expect("valid regex"));

/// Error count and distinct error codes scraped from a build log.
///
/// This is deliberately a regex over lines containing "error" and nothing
/// cleverer. It does not parse MSBuild, cargo, npm or anything else, and it
/// will be wrong in absolute terms on some toolchains. That is acceptable
/// because of what the number is used for: [`is_converging`] compares two
/// scans of the *same* command on the *same* repository, so a consistent
/// proxy is worth as much as a true count, and pretending to parse every
/// toolchain would buy accuracy nobody reads at a cost of correctness
/// everybody depends on.
///
/// Identical lines are counted once: MSBuild emits the same diagnostic once
/// per target that consumed the file, which would otherwise make the count
/// swing on project layout rather than on progress.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorScan {
    pub count: usize,
    pub codes: BTreeSet<String>,
}

pub fn scan_errors(output: &str) -> ErrorScan {
    let mut scan = ErrorScan::default();
    let mut seen: HashSet<&str> = HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || ERROR_TALLY.is_match(trimmed) || !ERROR_LINE.is_match(trimmed) {
            continue;
        }
        if !seen.insert(trimmed) {
            continue;
        }
        scan.count += 1;

        let mut coded = false;
        for caps in BRACKETED_ERROR
            .captures_iter(trimmed)
            .chain(CODED_ERROR.captures_iter(trimmed))
        {
            scan.codes.insert(caps[1].to_uppercase());
            coded = true;
        }
        if !coded {
            scan.codes.insert(GENERIC_ERROR_CODE.to_string());
        }
    }

    scan
}

/// One run of the acceptance command. `passed` is the whole verdict; the rest
/// exists so a human, and the convergence check, can see why.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Verification {
    pub command: String,
    pub passed: bool,
    /// `None` when the process was killed by a signal rather than exiting.
    pub exit_code: Option<i32>,
    pub error_count: usize,
    pub error_codes: BTreeSet<String>,
    /// Interleaved stdout and stderr, kept whole. Toolchains disagree about
    /// which stream diagnostics belong on, so both are scanned.
    pub output: String,
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", command]);
    cmd
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]);
    cmd
}

/// Run the acceptance command at `repo_root` and scrape its output.
///
/// A non-zero exit is a normal result, not an error — a failing test suite is
/// the case this module exists to handle. Only a command that cannot be
/// started at all fails here.
///
/// No timeout: `crate::tools::exec` owns timeouts and process-tree
/// termination for commands the *model* chooses, where a runaway is expected.
/// This command is one a human wrote into the plan, and silently truncating
/// the gate at an arbitrary deadline would report a failure that did not
/// happen.
pub fn run_acceptance(repo_root: &Path, command: &str) -> Result<Verification, IntegrateError> {
    if command.trim().is_empty() {
        return Err(IntegrateError::NoAcceptanceCommand);
    }

    let output = shell(command)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| IntegrateError::Acceptance {
            command: command.to_string(),
            source: source.into(),
        })?;

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !combined.ends_with('\n') && !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    let scan = scan_errors(&combined);
    Ok(Verification {
        command: command.to_string(),
        passed: output.status.success(),
        exit_code: output.status.code(),
        error_count: scan.count,
        error_codes: scan.codes,
        output: combined,
    })
}

// --- convergence -----------------------------------------------------------

/// Is the fixing making progress, judged from two consecutive verifications?
///
/// Two conditions, and the first is the one that earns its keep:
///
/// 1. **If any error code appears now that was not there before, the error
///    set has changed shape.** That is not progress and the caller should
///    escalate at once rather than spend the remaining cycle.
/// 2. Otherwise it converges only if the count actually fell. Unchanged is
///    not convergence — it is the same wall, hit twice.
///
/// The evidence, from `ORCHESTRATION.md`: one work item took three fix cycles
/// and never converged. Cycle 1 produced NU1015, cycle 2 produced NU1101
/// because a model had invented a NuGet package that does not exist, cycle 3
/// supplied the right package and surfaced 36 previously hidden code errors
/// underneath. The count never shrank and the shape kept moving, which is the
/// distinction that matters: **a shrinking error set is a job making
/// progress, a moving one is a job guessing**, and a guessing job costs money
/// per guess.
pub fn is_converging(previous: &Verification, current: &Verification) -> bool {
    if current
        .error_codes
        .difference(&previous.error_codes)
        .next()
        .is_some()
    {
        return false;
    }
    current.error_count < previous.error_count
}

/// What the caller should do with the level next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NextStep {
    /// Acceptance passed. The integration branch is the new base.
    Accept,
    /// Fix and verify again; a cycle has been spent.
    Redispatch,
    /// A human. Every remaining machine option costs money and changes
    /// nothing.
    Escalate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub step: NextStep,
    /// Plain prose, meant for the report and for the human it escalates to.
    pub reason: String,
}

// --- the level's integration state ----------------------------------------

/// Everything one level's integration did, and how it ended.
///
/// Held across fix cycles because the convergence check needs the previous
/// verification, and because a report has to say what happened, not what the
/// last step happened to return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Integration {
    pub branch: String,
    pub merged: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub verifications: Vec<Verification>,
    pub fix_cycles_used: u32,
    /// Set once, when the level stops needing a machine. `None` means it has
    /// not escalated.
    pub escalation: Option<String>,
}

impl Integration {
    pub fn new(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            merged: Vec::new(),
            conflicts: Vec::new(),
            verifications: Vec::new(),
            fix_cycles_used: 0,
            escalation: None,
        }
    }

    /// True once the acceptance command has passed on the integration branch.
    pub fn accepted(&self) -> bool {
        self.verifications.last().is_some_and(|v| v.passed)
    }

    /// Merge `branches` into the integration branch, in the order given.
    ///
    /// Checks the integration branch out first, so the caller cannot merge
    /// into whatever happened to be on HEAD. A branch that conflicts is
    /// recorded, the merge is aborted, and the remaining branches are still
    /// attempted — one bad partition should not hide the state of the others,
    /// which is exactly what a human triaging the escalation needs to see.
    pub fn merge_level(
        &mut self,
        repo_root: &Path,
        branches: &[String],
    ) -> Result<(), IntegrateError> {
        run_git(repo_root, &["checkout", "-q", &self.branch])?;

        for branch in branches {
            let (ok, _, stderr) = git(
                repo_root,
                &["merge", "--no-ff", "--no-edit", branch.as_str()],
            )?;
            if ok {
                self.merged.push(branch.clone());
                continue;
            }

            let files = unmerged_paths(repo_root)?;
            if files.is_empty() {
                // No unmerged paths means this was not a conflict — a bad ref,
                // an already-dirty tree, a repository problem. Nothing here
                // can act on it, so it surfaces as an error rather than being
                // filed as a conflict it is not.
                let _ = git(repo_root, &["merge", "--abort"]);
                return Err(IntegrateError::Git {
                    args: format!("merge --no-ff --no-edit {branch}"),
                    source: anyhow::anyhow!("{stderr}"),
                });
            }

            // Abort rather than resolve: the tree must be left usable for the
            // branches still queued behind this one.
            run_git(repo_root, &["merge", "--abort"])?;
            self.conflicts.push(Conflict {
                branch: branch.clone(),
                files,
            });
        }

        Ok(())
    }

    /// Record a verification and decide what happens next.
    ///
    /// The order of the checks is the policy:
    ///
    /// 1. A conflict is terminal — the partitioning was wrong, and no amount
    ///    of fixing inside a partition addresses that.
    /// 2. A pass is a pass, whatever the log said.
    /// 3. A shape shift escalates immediately, without spending a cycle it
    ///    has no reason to expect anything from.
    /// 4. A shrinking error set earns another cycle, up to
    ///    [`MAX_FIX_CYCLES`]. Then a human.
    pub fn record_verification(&mut self, verification: Verification) -> Decision {
        let previous = self.verifications.last().cloned();
        let passed = verification.passed;
        self.verifications.push(verification);

        if !self.conflicts.is_empty() {
            let branches: Vec<&str> = self.conflicts.iter().map(|c| c.branch.as_str()).collect();
            return self.escalate(format!(
                "merge conflicts on {} — resolution is a human's, and a conflict \
                 means two items in one level shared files",
                branches.join(", ")
            ));
        }

        if passed {
            return Decision {
                step: NextStep::Accept,
                reason: "acceptance command exited zero on the integration branch".to_string(),
            };
        }

        let current = self.verifications.last().expect("just pushed");

        if let Some(previous) = previous.as_ref()
            && !is_converging(previous, current)
        {
            let novel: Vec<&str> = current
                .error_codes
                .difference(&previous.error_codes)
                .map(String::as_str)
                .collect();
            let reason = if novel.is_empty() {
                format!(
                    "not converging: {} errors before, {} now — the same wall, hit twice",
                    previous.error_count, current.error_count
                )
            } else {
                format!(
                    "not converging: new error codes {} appeared, so the error set is \
                     moving rather than shrinking",
                    novel.join(", ")
                )
            };
            return self.escalate(reason);
        }

        if self.fix_cycles_used >= MAX_FIX_CYCLES {
            return self.escalate(format!(
                "fix cycle cap of {MAX_FIX_CYCLES} reached with {} errors outstanding",
                current.error_count
            ));
        }

        self.fix_cycles_used += 1;
        Decision {
            step: NextStep::Redispatch,
            reason: format!(
                "fix cycle {} of {MAX_FIX_CYCLES}: {} errors ({})",
                self.fix_cycles_used,
                current.error_count,
                current
                    .error_codes
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn escalate(&mut self, reason: String) -> Decision {
        if self.escalation.is_none() {
            self.escalation = Some(reason.clone());
        }
        Decision {
            step: NextStep::Escalate,
            reason,
        }
    }
}

/// Paths git left unmerged. Empty means the merge failed for some reason
/// other than a conflict.
fn unmerged_paths(repo_root: &Path) -> Result<Vec<String>, IntegrateError> {
    let out = run_git(repo_root, &["diff", "--name-only", "--diff-filter=U"])?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_repo(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "codemason-integrate-test-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        git(dir, args).map(|(ok, _, _)| ok).unwrap_or(false)
    }

    /// A repository with one commit, returning its default branch name —
    /// `git init` picks `master` or `main` depending on version and config,
    /// so nothing here may assume either.
    fn init_repo(dir: &Path) -> String {
        assert!(git_ok(dir, &["init", "-q"]));
        assert!(git_ok(dir, &["config", "user.email", "test@example.com"]));
        assert!(git_ok(dir, &["config", "user.name", "test"]));
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        assert!(git_ok(dir, &["add", "-A"]));
        assert!(git_ok(dir, &["commit", "-q", "-m", "initial"]));
        run_git(dir, &["branch", "--show-current"]).unwrap()
    }

    fn branch_with(dir: &Path, from: &str, branch: &str, file: &str, contents: &str) {
        assert!(git_ok(dir, &["checkout", "-q", from]));
        assert!(git_ok(dir, &["checkout", "-q", "-b", branch]));
        fs::write(dir.join(file), contents).unwrap();
        assert!(git_ok(dir, &["add", "-A"]));
        assert!(git_ok(dir, &["commit", "-q", "-m", branch]));
    }

    fn failing(count: usize, codes: &[&str]) -> Verification {
        Verification {
            command: "build".to_string(),
            passed: false,
            exit_code: Some(1),
            error_count: count,
            error_codes: codes.iter().map(|c| c.to_string()).collect(),
            output: String::new(),
        }
    }

    #[test]
    fn two_disjoint_branches_merge_clean() {
        let dir = temp_repo("clean-merge");
        let main = init_repo(&dir);
        branch_with(&dir, &main, "codemason/a", "a.txt", "from a\n");
        branch_with(&dir, &main, "codemason/b", "b.txt", "from b\n");

        create_integration_branch(&dir, "codemason/integration", &main).unwrap();
        let mut integration = Integration::new("codemason/integration");
        integration
            .merge_level(
                &dir,
                &["codemason/a".to_string(), "codemason/b".to_string()],
            )
            .unwrap();

        assert_eq!(integration.merged.len(), 2);
        assert!(integration.conflicts.is_empty());
        assert!(dir.join("a.txt").exists());
        assert!(dir.join("b.txt").exists());
    }

    #[test]
    fn a_conflict_is_recorded_and_the_merge_aborted() {
        let dir = temp_repo("conflict");
        let main = init_repo(&dir);
        branch_with(&dir, &main, "codemason/a", "shared.txt", "written by a\n");
        branch_with(&dir, &main, "codemason/b", "shared.txt", "written by b\n");

        create_integration_branch(&dir, "codemason/integration", &main).unwrap();
        let mut integration = Integration::new("codemason/integration");
        integration
            .merge_level(
                &dir,
                &["codemason/a".to_string(), "codemason/b".to_string()],
            )
            .unwrap();

        assert_eq!(integration.merged, vec!["codemason/a".to_string()]);
        assert_eq!(integration.conflicts.len(), 1);
        assert_eq!(integration.conflicts[0].branch, "codemason/b");
        assert_eq!(integration.conflicts[0].files, vec!["shared.txt".to_string()]);

        // The tree must be usable afterwards: no merge in progress, nothing
        // left unmerged, and no conflict markers on disk.
        assert!(!dir.join(".git").join("MERGE_HEAD").exists());
        assert!(unmerged_paths(&dir).unwrap().is_empty());
        let status = run_git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(status.is_empty(), "tree should be clean, got: {status}");
        let shared = fs::read_to_string(dir.join("shared.txt")).unwrap();
        assert!(!shared.contains("<<<<<<<"));
    }

    #[test]
    fn a_conflict_escalates_rather_than_being_fixed() {
        let mut integration = Integration::new("codemason/integration");
        integration.conflicts.push(Conflict {
            branch: "codemason/b".to_string(),
            files: vec!["shared.txt".to_string()],
        });

        let decision = integration.record_verification(failing(3, &["CS0246"]));
        assert_eq!(decision.step, NextStep::Escalate);
        assert_eq!(integration.fix_cycles_used, 0);
        assert!(integration.escalation.is_some());
    }

    #[test]
    fn a_new_error_code_is_a_shape_shift_and_does_not_converge() {
        // The measured case: NU1015 became NU1101 when a model invented a
        // package that does not exist.
        let before = failing(1, &["NU1015"]);
        let after = failing(1, &["NU1101"]);
        assert!(!is_converging(&before, &after));

        // Still a shape shift even when the count fell.
        let fewer_but_different = failing(0, &["NU1101"]);
        assert!(!is_converging(&before, &fewer_but_different));
    }

    #[test]
    fn a_shrinking_count_with_the_same_codes_converges() {
        let before = failing(36, &["CS0246", "CS0103"]);
        let after = failing(12, &["CS0246"]);
        assert!(is_converging(&before, &after));
    }

    #[test]
    fn an_unchanged_count_does_not_converge() {
        let before = failing(4, &["CS0246"]);
        let after = failing(4, &["CS0246"]);
        assert!(!is_converging(&before, &after));
        // Nor does a growing one.
        assert!(!is_converging(&before, &failing(9, &["CS0246"])));
    }

    #[test]
    fn a_shape_shift_escalates_without_spending_the_remaining_cycle() {
        let mut integration = Integration::new("i");
        assert_eq!(
            integration.record_verification(failing(1, &["NU1015"])).step,
            NextStep::Redispatch
        );
        assert_eq!(integration.fix_cycles_used, 1);

        let decision = integration.record_verification(failing(1, &["NU1101"]));
        assert_eq!(decision.step, NextStep::Escalate);
        assert!(decision.reason.contains("NU1101"));
        // The second cycle was never spent — that is the point of the check.
        assert_eq!(integration.fix_cycles_used, 1);
    }

    #[test]
    fn the_fix_cycle_cap_is_respected() {
        let mut integration = Integration::new("i");
        // Converging every time, so only the cap can stop it.
        assert_eq!(
            integration.record_verification(failing(30, &["CS0246"])).step,
            NextStep::Redispatch
        );
        assert_eq!(
            integration.record_verification(failing(20, &["CS0246"])).step,
            NextStep::Redispatch
        );
        let decision = integration.record_verification(failing(10, &["CS0246"]));
        assert_eq!(decision.step, NextStep::Escalate);
        assert_eq!(integration.fix_cycles_used, MAX_FIX_CYCLES);
        assert!(integration.escalation.unwrap().contains("cap"));
    }

    #[test]
    fn a_pass_is_accepted_whatever_the_log_said() {
        let mut integration = Integration::new("i");
        let verification = Verification {
            command: "build".to_string(),
            passed: true,
            exit_code: Some(0),
            // A warning line mentioning "error" is not a failing gate.
            error_count: 1,
            error_codes: BTreeSet::from(["ERROR".to_string()]),
            output: String::new(),
        };
        let decision = integration.record_verification(verification);
        assert_eq!(decision.step, NextStep::Accept);
        assert!(integration.accepted());
    }

    #[test]
    fn msbuild_and_nuget_lines_yield_their_codes() {
        let output = "\
Foo.cs(4,41): error CS0246: The type or namespace name 'X' could not be found
error NU1101: Unable to find package Y
Bar.cs(9,3): error CS0103: The name 'z' does not exist in the current context
    2 Error(s)
";
        let scan = scan_errors(output);
        assert_eq!(scan.count, 3, "the Error(s) tally must not be counted");
        assert_eq!(
            scan.codes,
            BTreeSet::from([
                "CS0246".to_string(),
                "CS0103".to_string(),
                "NU1101".to_string()
            ])
        );
    }

    #[test]
    fn an_unknown_toolchain_still_yields_a_comparable_count() {
        let output = "\
running tests
error: something went wrong in a toolchain we do not parse
error[E0308]: mismatched types
error: something went wrong in a toolchain we do not parse
all done
";
        let scan = scan_errors(output);
        // The repeated identical line is counted once.
        assert_eq!(scan.count, 2);
        assert!(scan.codes.contains("E0308"));
        assert!(scan.codes.contains(GENERIC_ERROR_CODE));
    }

    #[test]
    fn clean_output_scans_as_no_errors() {
        let scan = scan_errors("Build succeeded.\n    0 Error(s)\n    0 Warning(s)\n");
        assert_eq!(scan, ErrorScan::default());
    }

    #[test]
    fn the_acceptance_exit_status_is_the_gate() {
        let dir = temp_repo("acceptance");
        init_repo(&dir);

        let failed = run_acceptance(&dir, "echo Foo.cs(4,41): error CS0246: nope && exit 1")
            .expect("the command should start");
        assert!(!failed.passed);
        assert_eq!(failed.exit_code, Some(1));
        assert!(failed.error_codes.contains("CS0246"));

        let passed = run_acceptance(&dir, "echo ok").expect("the command should start");
        assert!(passed.passed);
        assert_eq!(passed.error_count, 0);

        assert!(matches!(
            run_acceptance(&dir, "   "),
            Err(IntegrateError::NoAcceptanceCommand)
        ));
    }

    #[test]
    fn branches_merge_in_level_order_and_unmergeable_runs_are_skipped() {
        fn completed(id: &str, level: u32, exit_code: i32, branch: Option<&str>) -> CompletedItem {
            CompletedItem {
                item: WorkItem {
                    id: id.to_string(),
                    partition_id: None,
                    level,
                    repo: ".".to_string(),
                    task: "t".to_string(),
                    acceptance: Some("build".to_string()),
                },
                outcome: RunOutcome {
                    exit_code,
                    branch: branch.map(str::to_string),
                    ..Default::default()
                },
            }
        }

        let items = vec![
            completed("c", 2, 0, Some("codemason/c")),
            completed("a", 1, 0, Some("codemason/a")),
            // Exit 3 hit the iteration ceiling but committed its work.
            completed("b", 1, 3, Some("codemason/b")),
            // Gated: nothing on the branch is trustworthy.
            completed("d", 1, 4, Some("codemason/d")),
            completed("e", 1, 0, None),
        ];

        assert_eq!(
            merge_order(&items),
            vec![
                "codemason/a".to_string(),
                "codemason/b".to_string(),
                "codemason/c".to_string()
            ]
        );
    }
}
