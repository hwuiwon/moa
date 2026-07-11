//! Hosted eval service wire DTOs.

use crate::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for planning an eval suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPlanRequest {
    /// Tenant used for authorization and eval execution.
    pub tenant_id: TenantId,
    /// Raw suite document supplied by the API caller.
    pub suite_document: String,
    /// Logical suite source path or URI.
    pub suite_source: Option<String>,
    /// Raw agent configuration documents supplied by the API caller.
    #[serde(default)]
    pub config_documents: Vec<String>,
    /// Logical config source paths or URIs.
    #[serde(default)]
    pub config_sources: Vec<String>,
}

/// Response payload describing an eval execution plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPlanResponse {
    /// Suite name that would be executed.
    pub suite_name: String,
    /// Agent config names included in the run.
    #[serde(default)]
    pub configs: Vec<String>,
    /// Test case names included in the run.
    #[serde(default)]
    pub cases: Vec<String>,
    /// Total `(config, case)` executions.
    pub total_runs: u64,
    /// Coarse minimum estimated dollar cost.
    pub estimated_min_cost_dollars: f64,
    /// Coarse maximum estimated dollar cost.
    pub estimated_max_cost_dollars: f64,
}

/// One eval suite document supplied for hosted listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListDocument {
    /// Logical suite source path or URI.
    pub source: Option<String>,
    /// Raw suite TOML document.
    pub body: String,
}

/// Request payload for listing eval suite summaries from supplied documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Suite documents to parse and summarize.
    #[serde(default)]
    pub documents: Vec<EvalSuiteListDocument>,
}

/// Hosted eval suite summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteSummary {
    /// Logical suite source path or URI.
    pub source: Option<String>,
    /// Stable suite name.
    pub name: String,
    /// Number of cases in the suite.
    pub cases: u64,
    /// Optional suite description.
    pub description: Option<String>,
    /// Suite tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Response payload for listing eval suite summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListResponse {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Parsed suite summaries ordered like the request documents.
    #[serde(default)]
    pub suites: Vec<EvalSuiteSummary>,
}

/// Request payload for running an eval suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunRequest {
    /// Tenant used for authorization and eval execution.
    pub tenant_id: TenantId,
    /// Raw suite document supplied by the API caller.
    pub suite_document: String,
    /// Logical suite source path or URI.
    pub suite_source: Option<String>,
    /// Raw agent configuration documents supplied by the API caller.
    #[serde(default)]
    pub config_documents: Vec<String>,
    /// Logical config source paths or URIs.
    #[serde(default)]
    pub config_sources: Vec<String>,
    /// Report sink specs such as `terminal` or `json:<path>`.
    #[serde(default)]
    pub reports: Vec<String>,
    /// Maximum concurrent eval executions.
    pub parallel: u32,
    /// Whether CI exit-code semantics should be applied.
    #[serde(default)]
    pub ci: bool,
    /// Evaluator names to run.
    #[serde(default)]
    pub evaluators: Vec<String>,
    /// Maximum allowed per-run cost in dollars.
    pub max_cost_dollars: Option<f64>,
    /// Maximum allowed per-run latency in milliseconds.
    pub max_latency_ms: Option<u64>,
    /// Maximum allowed tokens per run.
    pub max_tokens: Option<u64>,
    /// Maximum allowed tool calls per run.
    pub max_tool_calls: Option<u64>,
    /// Maximum allowed turns per run.
    pub max_turns: Option<u64>,
    /// Whether per-case response and score comments should be included.
    #[serde(default)]
    pub verbose: bool,
}

/// Response payload for an eval suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunResponse {
    /// Tenant used for authorization and eval execution.
    pub tenant_id: TenantId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
    /// Current run lifecycle status.
    pub status: EvalRunStatus,
    /// Suite name that was executed.
    pub suite_name: String,
    /// Process exit code recommended for automation.
    pub exit_code: i32,
    /// Aggregate run summary.
    pub summary: Value,
    /// Per-case eval results.
    #[serde(default)]
    pub results: Vec<Value>,
    /// Terminal error when the hosted eval failed before producing case results.
    pub error: Option<String>,
}

/// Durable server-side lifecycle status for an eval run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunStatus {
    /// The run has been accepted but has not started visible work.
    #[default]
    Pending,
    /// The run is executing on the hosted orchestrator.
    Running,
    /// The run completed and contains terminal results.
    Completed,
    /// The run failed before producing terminal results.
    Failed,
}

/// Request payload for polling an eval run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunStatusRequest {
    /// Tenant used for authorization and run-result filtering.
    pub tenant_id: TenantId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
}

/// Response payload for polling an eval run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunStatusResponse {
    /// Tenant that owns this run.
    pub tenant_id: TenantId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
    /// Current run lifecycle status.
    pub status: EvalRunStatus,
    /// Suite name when known.
    pub suite_name: Option<String>,
    /// Process exit code recommended for automation once terminal.
    pub exit_code: Option<i32>,
    /// Aggregate run summary once terminal.
    pub summary: Option<Value>,
    /// Per-case eval results once terminal.
    #[serde(default)]
    pub results: Vec<Value>,
    /// Terminal error when the hosted eval failed before producing case results.
    pub error: Option<String>,
}

/// Request payload for registering an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetRegisterRequest {
    /// Tenant used for authorization and dataset item ownership.
    pub tenant_id: TenantId,
    /// Dataset name.
    pub name: String,
    /// Raw JSONL dataset content.
    pub jsonl: String,
    /// Logical source path or URI for the dataset.
    pub source_uri: Option<String>,
}

/// Response payload for registering an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetRegisterResponse {
    /// Tenant that owns the registered dataset items.
    pub tenant_id: TenantId,
    /// Registered dataset identifier.
    pub dataset_id: Uuid,
    /// Dataset name.
    pub name: String,
    /// Number of dataset items registered.
    pub items: u64,
}

/// Request payload for listing eval datasets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDatasetListRequest {
    /// Tenant used for authorization and dataset filtering.
    pub tenant_id: TenantId,
}

/// Tenant-scoped eval dataset summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDatasetSummary {
    /// Tenant that has items in this dataset.
    pub tenant_id: TenantId,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Dataset name.
    pub name: String,
    /// Number of items visible in this tenant.
    pub items: u64,
    /// Logical source path or URI for the dataset.
    pub source_uri: Option<String>,
}

/// Response payload for listing eval datasets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetListResponse {
    /// Tenant used to filter dataset item counts.
    pub tenant_id: TenantId,
    /// Dataset summaries ordered for API display.
    #[serde(default)]
    pub datasets: Vec<EvalDatasetSummary>,
}

/// Request payload for replaying an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReplayRequest {
    /// Tenant used for authorization and dataset item filtering.
    pub tenant_id: TenantId,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Optional replay run identifier.
    pub run_id: Option<Uuid>,
    /// Maximum dataset items to replay.
    pub limit: Option<u64>,
    /// Optional embedder label for the run.
    pub embedder: Option<String>,
    /// Optional model label for the run.
    pub model: Option<String>,
}

/// Response payload for replaying an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReplayResponse {
    /// Tenant used for dataset item filtering.
    pub tenant_id: TenantId,
    /// Replay run identifier.
    pub run_id: Uuid,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Number of dataset items processed.
    pub items: u64,
    /// Number of score rows emitted.
    pub scores: u64,
}

/// Request payload for reading eval score summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoresRequest {
    /// Tenant used for authorization and score filtering.
    pub tenant_id: TenantId,
    /// Replay run identifier.
    pub run_id: Uuid,
}

/// Tenant-scoped eval score summary row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoreSummaryRow {
    /// Score name.
    pub name: String,
    /// Score value type.
    pub value_type: String,
    /// Number of rows summarized.
    pub n: u64,
    /// Numeric mean or boolean true-rate, or `None` when every summarized value is NULL.
    pub mean_or_rate: Option<f64>,
}

/// Response payload containing eval score summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoresResponse {
    /// Tenant used for score filtering.
    pub tenant_id: TenantId,
    /// Replay run identifier.
    pub run_id: Uuid,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<EvalScoreSummaryRow>,
}

/// Request payload for comparing two eval replay runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareRequest {
    /// Tenant used for authorization and score filtering.
    pub tenant_id: TenantId,
    /// Baseline replay run identifier.
    pub base_run: Uuid,
    /// New replay run identifier.
    pub new_run: Uuid,
}

/// Tenant-scoped eval run comparison row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareRow {
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean when both sides have data.
    pub delta: Option<f64>,
}

/// Response payload containing eval run comparison rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareResponse {
    /// Tenant used for score filtering.
    pub tenant_id: TenantId,
    /// Baseline replay run identifier.
    pub base_run: Uuid,
    /// New replay run identifier.
    pub new_run: Uuid,
    /// Comparison rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<EvalCompareRow>,
}
