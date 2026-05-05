//! Result and metrics types produced by MOA evaluation runs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Outcome of running one test case against one agent configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalResult {
    /// Name of the test case that was executed.
    pub test_case: String,
    /// Name of the agent configuration used for the run.
    pub agent_config: String,
    /// Final pass/fail/error status for the run.
    pub status: EvalStatus,
    /// Final agent response text, when one was produced.
    pub response: Option<String>,
    /// Actual trajectory collected during execution.
    pub trajectory: Vec<TrajectoryStep>,
    /// Scores returned by evaluators.
    pub scores: Vec<EvalScore>,
    /// Aggregate metrics collected during execution.
    pub metrics: EvalMetrics,
    /// Trace identifier for linking to external observability systems.
    pub trace_id: Option<String>,
    /// Terminal error message, when the run errored.
    pub error: Option<String>,
    /// Start timestamp for the run.
    pub started_at: DateTime<Utc>,
    /// Completion timestamp for the run.
    pub completed_at: DateTime<Utc>,
}

impl Default for EvalResult {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            test_case: String::new(),
            agent_config: String::new(),
            status: EvalStatus::default(),
            response: None,
            trajectory: Vec::new(),
            scores: Vec::new(),
            metrics: EvalMetrics::default(),
            trace_id: None,
            error: None,
            started_at: now,
            completed_at: now,
        }
    }
}

/// Final status for an evaluation run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    /// The run met its evaluation criteria.
    Passed,
    /// The run completed but did not meet its evaluation criteria.
    Failed,
    /// The agent errored before completing.
    Error,
    /// The run exceeded its allotted timeout.
    Timeout,
    /// The run was skipped.
    #[default]
    Skipped,
}

/// One tool or action step observed during execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TrajectoryStep {
    /// Tool name that was invoked.
    pub tool_name: String,
    /// Truncated summary of the tool input.
    pub input_summary: String,
    /// Truncated summary of the tool output.
    pub output_summary: String,
    /// Whether the step completed successfully.
    pub success: bool,
    /// Duration of the step in milliseconds.
    pub duration_ms: u64,
}

/// Aggregate quantitative metrics for a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvalMetrics {
    /// Total tokens consumed across the run.
    pub total_tokens: usize,
    /// Input tokens consumed across the run.
    pub input_tokens: usize,
    /// Output tokens consumed across the run.
    pub output_tokens: usize,
    /// Estimated cost in dollars.
    pub cost_dollars: f64,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Number of turns taken by the agent.
    pub turn_count: usize,
    /// Number of tool calls issued by the agent.
    pub tool_call_count: usize,
    /// Number of tool errors observed.
    pub tool_error_count: usize,
}

/// A score emitted by an evaluator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScore {
    /// Evaluator name that produced the score.
    pub evaluator: String,
    /// Score name within the evaluator.
    pub name: String,
    /// Score value.
    pub value: ScoreValue,
    /// Optional evaluator comment or reasoning.
    pub comment: Option<String>,
}

/// Score value emitted by an evaluator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreValue {
    /// A numeric score, typically in the range `0.0..=1.0`.
    Numeric(f64),
    /// A boolean score.
    Boolean(bool),
    /// A categorical score label.
    Categorical(String),
}
