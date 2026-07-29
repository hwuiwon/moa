//! Shared eval run options and result aggregates.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::admission::EvalAdmissionLimits;
use crate::resource_report::RunResourceReport;
use crate::{EvalResult, EvalStatus};

/// Options that control eval execution behavior.
///
/// `parallel` is not a hint: it is checked against
/// [`EvalAdmissionLimits::max_parallel_cases`] and a request above the bound is
/// rejected rather than reduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineOptions {
    /// Maximum number of cases to execute concurrently.
    pub parallel: usize,
    /// Base directory used for temporary eval workspaces.
    pub temp_dir: PathBuf,
    /// When true, skip execution and mark runs as skipped.
    pub dry_run: bool,
    /// Whether to capture response and tool content in results.
    pub capture_content: bool,
    /// Maximum bytes captured for any text payload.
    pub content_max_bytes: usize,
    /// Hard maximums enforced before any case is dispatched.
    pub admission: EvalAdmissionLimits,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            parallel: 1,
            temp_dir: std::env::temp_dir().join("moa-eval"),
            dry_run: false,
            capture_content: true,
            content_max_bytes: 32 * 1024,
            admission: EvalAdmissionLimits::default(),
        }
    }
}

/// Complete suite execution result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    /// Suite name that was executed.
    pub suite_name: String,
    /// Wall-clock start time for the run.
    pub started_at: DateTime<Utc>,
    /// Wall-clock completion time for the run.
    pub completed_at: DateTime<Utc>,
    /// Per `(config, case)` result entries.
    pub results: Vec<EvalResult>,
    /// Aggregate summary across all results.
    pub summary: RunSummary,
    /// Reservation accounting for the run, when the engine enforced an envelope.
    #[serde(default)]
    pub resources: Option<RunResourceReport>,
}

/// Aggregate counters and resource usage across a suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RunSummary {
    /// Total number of `(config, case)` runs.
    pub total_cases: usize,
    /// Number of successful executions.
    pub passed: usize,
    /// Number of failed evals.
    pub failed: usize,
    /// Number of errored runs.
    pub errors: usize,
    /// Number of timed-out runs.
    pub timeouts: usize,
    /// Total tokens consumed across all runs.
    pub total_tokens: usize,
    /// Total estimated dollar cost across all runs.
    pub total_cost_dollars: f64,
    /// Total wall-clock execution duration in milliseconds.
    pub total_duration_ms: u64,
}

impl RunSummary {
    /// Aggregates a summary from a list of results.
    #[must_use]
    pub fn from_results(results: &[EvalResult]) -> Self {
        let mut summary = Self {
            total_cases: results.len(),
            ..Self::default()
        };

        for result in results {
            match result.status {
                EvalStatus::Passed => summary.passed += 1,
                EvalStatus::Failed => summary.failed += 1,
                EvalStatus::Error => summary.errors += 1,
                EvalStatus::Timeout => summary.timeouts += 1,
                EvalStatus::Skipped => {}
            }
            summary.total_tokens += result.metrics.total_tokens;
            summary.total_cost_dollars += result.metrics.cost_dollars;
            summary.total_duration_ms += (result.completed_at - result.started_at)
                .num_milliseconds()
                .max(0) as u64;
        }

        summary
    }
}
