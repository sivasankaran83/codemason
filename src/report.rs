//! `RunReport` — the single JSON object `codemason run` writes to stdout on
//! every exit path (SPEC.md T4.4 / AC9). Every other line of output goes to
//! stderr; this is the whole stdout contract.

use std::time::Instant;

use serde::Serialize;
use uuid::Uuid;

use crate::cli::ExitCode;
use crate::text;

#[derive(Debug, Clone, Serialize, Default)]
pub struct IndexReport {
    pub chunk_count: usize,
    pub build_ms: u128,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct TotalsReport {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub status: &'static str,
    pub exit_code: i32,
    /// The model's own account of what it did — the terminating assistant
    /// message, which is the loop's definition of "finished". Present only
    /// on a completed run: a run that breached a budget or ceiling never
    /// produced one, and inventing a summary for it would be reporting work
    /// nobody described. `null` there is the honest value.
    ///
    /// This is prose from a model and must be treated as such by anything
    /// reading it: useful for a human deciding whether to look closer, and
    /// for a supervisor to log, but never evidence that the work is correct.
    /// `files_changed` and `commit` are the facts; this is the claim.
    pub summary: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub files_changed: Vec<String>,
    pub iterations: u32,
    pub index: Option<IndexReport>,
    pub models_used: Vec<String>,
    pub totals: TotalsReport,
    pub duration_ms: u128,
    pub log_path: Option<String>,
}

impl RunReport {
    /// A fresh report with `run_id` as the only thing known — every other
    /// field defaults to "not reached yet" so the earliest possible failure
    /// can still be reported as one valid JSON object.
    pub fn new(run_id: Uuid) -> Self {
        Self {
            run_id: run_id.to_string(),
            status: "unrecoverable_error",
            exit_code: ExitCode::UnrecoverableError.into(),
            summary: None,
            branch: None,
            commit: None,
            files_changed: Vec::new(),
            iterations: 0,
            index: None,
            models_used: Vec::new(),
            totals: TotalsReport::default(),
            duration_ms: 0,
            log_path: None,
        }
    }
}

fn status_for(code: ExitCode) -> &'static str {
    match code {
        ExitCode::Completed => "completed",
        ExitCode::UnrecoverableError => "unrecoverable_error",
        ExitCode::BudgetExceeded => "budget_exceeded",
        ExitCode::MaxIterationsExceeded => "max_iterations_exceeded",
        ExitCode::ModelGated => "model_gated",
        ExitCode::ProviderError => "provider_error",
    }
}

/// Finalize `report` for `code`, write it as exactly one JSON line to
/// stdout, and return `code` unchanged so call sites can
/// `return finish(report, ExitCode::X, start);`.
pub fn finish(mut report: RunReport, code: ExitCode, start: Instant) -> ExitCode {
    report.status = status_for(code);
    report.exit_code = code.into();
    report.duration_ms = start.elapsed().as_millis();

    let line = serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string());
    let _ = text::write_stdout(&format!("{line}\n"));

    code
}
