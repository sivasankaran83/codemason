//! The supervisor above `codemason` — stages 3 to 6 of `ORCHESTRATION.md`.
//!
//! `codemason` is the executor and knows nothing about any of this. The
//! supervisor drives it as a process and reads its stdout JSON and exit code;
//! that contract is the entire interface between the two, and nothing here
//! reaches inside the binary.
//!
//! The loop follows AgentFlow (arXiv:2510.05592): **planner, executor,
//! verifier, generator**, coordinated by an **evolving memory** that persists
//! across cycles. Only the architecture is borrowed — nothing here trains a
//! planner, so none of that paper's reported gains are claimed.
//!
//! Why the memory is load-bearing rather than decorative: every `codemason`
//! process is stateless by design. It takes a task string, works, commits and
//! exits, remembering nothing. Two consecutive attempts at a failing item
//! therefore begin equally blind unless something above them accumulates what
//! was learned — and in the session that motivated this design, that something
//! was a human.

pub mod execute;
pub mod integrate;
pub mod memory;
pub mod plan;

use serde::{Deserialize, Serialize};

/// Fix cycles per item before escalating to a human.
///
/// Two, on evidence: an item that took three cycles never converged, and the
/// error set was shifting rather than shrinking by the second. See
/// `ORCHESTRATION.md`, "Fix cycles are capped at two".
pub const MAX_FIX_CYCLES: u32 = 2;

/// One unit of work for a single `codemason` process.
///
/// `task` is the entire context the job will ever have: it sees this string
/// and its own repository, nothing else. Anything the planner knows and does
/// not write here is knowledge the job does not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    /// Which partition owns the files this item touches. Two items in one
    /// level must never share a partition.
    #[serde(default)]
    pub partition_id: Option<String>,
    /// Items in the same level run concurrently; a later level starts only
    /// once every item in the previous one has finished.
    pub level: u32,
    pub repo: String,
    pub task: String,
    /// The command that proves the item worked. This, not the model's
    /// summary, is what decides whether the item is done.
    #[serde(default)]
    pub acceptance: Option<String>,
}

/// A plan is the planner module's output: work items sorted into levels.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Plan {
    pub items: Vec<WorkItem>,
    #[serde(default)]
    pub rationale: String,
    /// What the planner deliberately did not split, and why. Recorded because
    /// "this could not usefully be parallelised" is a legitimate outcome and
    /// should not look like an omission.
    #[serde(default)]
    pub not_parallelized: Option<String>,
}

impl Plan {
    /// Levels present, in execution order.
    pub fn levels(&self) -> Vec<u32> {
        let mut ls: Vec<u32> = self.items.iter().map(|i| i.level).collect();
        ls.sort_unstable();
        ls.dedup();
        ls
    }

    pub fn items_at(&self, level: u32) -> Vec<&WorkItem> {
        self.items.iter().filter(|i| i.level == level).collect()
    }

    /// Two items in one level sharing a partition is a planning error: they
    /// can edit the same files concurrently, which is the exact failure
    /// partitioning exists to prevent. Returns the offending partition ids.
    pub fn conflicting_partitions(&self) -> Vec<String> {
        let mut bad = Vec::new();
        for level in self.levels() {
            let mut seen: Vec<&str> = Vec::new();
            for item in self.items_at(level) {
                let Some(p) = item.partition_id.as_deref() else {
                    continue;
                };
                if seen.contains(&p) {
                    let owned = p.to_string();
                    if !bad.contains(&owned) {
                        bad.push(owned);
                    }
                } else {
                    seen.push(p);
                }
            }
        }
        bad
    }
}

/// `codemason run`'s stdout report, as the supervisor reads it.
///
/// Deliberately a separate type from `crate::report::RunReport` rather than
/// deserialising that one: this is a wire contract between two processes, and
/// making the reader tolerant of fields it does not know keeps a supervisor
/// working against a newer `codemason` than it was built with.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct RunOutcome {
    #[serde(default)]
    pub run_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub exit_code: i32,
    /// The model's own account of what it did. Useful to log and to show a
    /// human. Never evidence — `files_changed`, `commit` and the acceptance
    /// command are what actually happened.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub iterations: u32,
    #[serde(default)]
    pub totals: Totals,
    #[serde(default)]
    pub duration_ms: u128,
    #[serde(default)]
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Totals {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub cost: f64,
}

/// What the supervisor should do next with an item, decided from the exit
/// code alone. The exit code says *how a run stopped*, not *whether the work
/// is done* — 2 and 3 commit their work, and whether that work is complete is
/// a question only the acceptance command can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Run the acceptance command; accept or re-dispatch on the result.
    Verify,
    /// Retry the same dispatch after a backoff, then escalate.
    RetryTransient,
    /// A human is needed. Retrying spends money without changing anything.
    Escalate,
}

pub fn disposition(exit_code: i32) -> Disposition {
    match exit_code {
        // Completed, or stopped on budget/ceiling with work committed. All
        // three are decided the same way: by running the tests.
        0 | 2 | 3 => Disposition::Verify,
        // Provider trouble. Worth one backoff, then a human.
        5 => Disposition::RetryTransient,
        // 1 unrecoverable, 4 model gated, anything unknown.
        _ => Disposition::Escalate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, level: u32, partition: Option<&str>) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            partition_id: partition.map(|s| s.to_string()),
            level,
            repo: ".".to_string(),
            task: "t".to_string(),
            acceptance: None,
        }
    }

    #[test]
    fn exit_two_and_three_are_verified_not_failed() {
        // The row that matters. A run that stopped on budget still committed
        // its work, and treating that as failure discards it.
        assert_eq!(disposition(0), Disposition::Verify);
        assert_eq!(disposition(2), Disposition::Verify);
        assert_eq!(disposition(3), Disposition::Verify);
    }

    #[test]
    fn gating_and_unrecoverable_escalate_rather_than_retry() {
        assert_eq!(disposition(1), Disposition::Escalate);
        assert_eq!(disposition(4), Disposition::Escalate);
        assert_eq!(disposition(99), Disposition::Escalate);
    }

    #[test]
    fn provider_errors_are_worth_one_retry() {
        assert_eq!(disposition(5), Disposition::RetryTransient);
    }

    #[test]
    fn levels_run_in_order_and_items_group_by_level() {
        let plan = Plan {
            items: vec![item("c", 3, None), item("a", 1, None), item("b", 1, None)],
            ..Default::default()
        };
        assert_eq!(plan.levels(), vec![1, 3]);
        assert_eq!(plan.items_at(1).len(), 2);
        assert_eq!(plan.items_at(3).len(), 1);
    }

    #[test]
    fn two_items_sharing_a_partition_in_one_level_is_a_planning_error() {
        let plan = Plan {
            items: vec![item("a", 1, Some("p0")), item("b", 1, Some("p0"))],
            ..Default::default()
        };
        assert_eq!(plan.conflicting_partitions(), vec!["p0".to_string()]);
    }

    #[test]
    fn the_same_partition_across_different_levels_is_fine() {
        // Sequential access to a partition is safe; only concurrent access
        // inside one level is the failure.
        let plan = Plan {
            items: vec![item("a", 1, Some("p0")), item("b", 2, Some("p0"))],
            ..Default::default()
        };
        assert!(plan.conflicting_partitions().is_empty());
    }

    #[test]
    fn an_unknown_field_does_not_break_reading_a_run_report() {
        // A supervisor must keep working against a newer codemason.
        let json = r#"{"run_id":"r","status":"completed","exit_code":0,
                       "files_changed":["a.rs"],"totals":{"total_tokens":5,"cost":0.1},
                       "some_future_field":true}"#;
        let out: RunOutcome = serde_json::from_str(json).expect("tolerant of unknown fields");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.files_changed, vec!["a.rs".to_string()]);
        assert_eq!(out.totals.cost, 0.1);
    }
}
