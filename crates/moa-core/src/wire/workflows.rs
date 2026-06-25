//! Workflow service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for starting an artifact-backed workflow run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunRequest {
    /// Tenant used for authorization and execution.
    pub tenant_id: TenantId,
    /// Workflow artifact reference, for example `workflow://damaged-food-order`.
    pub workflow_ref: String,
    /// Initial workflow input.
    #[serde(default)]
    pub input: Value,
    /// Optional session that should receive agent-loop work.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Optional idempotency key for run creation.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Response payload returned when a workflow run is started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunResponse {
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Initial run status.
    pub status: String,
}

/// Request payload for loading workflow run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStatusRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Workflow run row identifier.
    pub run_id: Uuid,
}

/// Response payload for workflow run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunStatus {
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Session associated with this workflow run, when present.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Current node ID, if execution has started.
    pub current_node_id: Option<String>,
    /// Current run status.
    pub status: String,
    /// Per-node run summaries.
    #[serde(default)]
    pub node_runs: Vec<WorkflowNodeRunSummary>,
    /// Terminal output payload.
    #[serde(default)]
    pub output: Option<Value>,
    /// Terminal error text.
    #[serde(default)]
    pub error: Option<String>,
}

/// Summary of one workflow node execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowNodeRunSummary {
    /// Workflow node ID.
    pub node_id: String,
    /// Node run status.
    pub status: String,
    /// Node start timestamp.
    pub started_at: DateTime<Utc>,
    /// Node completion timestamp.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Request payload for cancelling a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCancelRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Optional cancellation reason.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response payload returned after requesting workflow cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCancelResponse {
    /// Whether cancellation was accepted.
    pub cancelled: bool,
    /// Human-readable status message.
    pub reason: String,
}

/// Decision kind for a workflow review node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowReviewDecisionKind {
    /// Approve the waiting workflow review node.
    Approved,
    /// Reject the waiting workflow review node.
    Rejected,
}

/// Request payload for deciding a workflow review node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowReviewDecisionRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Review node to decide. Defaults to the run's current node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Decision to apply.
    pub decision: WorkflowReviewDecisionKind,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional approved output to store under the review node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// Response payload returned after deciding a workflow review node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowReviewDecisionResponse {
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Whether this request changed workflow state.
    pub accepted: bool,
    /// Current run status when the decision was accepted or rejected.
    pub status: String,
    /// Current node ID after the decision was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node_id: Option<String>,
}

/// Request payload for delivering an external workflow signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSignalRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Wait-signal node to resolve. Defaults to the run's current node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Optional logical signal name supplied by the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_name: Option<String>,
    /// Signal payload to store under the wait-signal node id.
    #[serde(default)]
    pub payload: Value,
}

/// Response payload returned after delivering an external workflow signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSignalResponse {
    /// Workflow run row identifier.
    pub run_id: Uuid,
    /// Whether this request was accepted by the waiting workflow.
    pub accepted: bool,
    /// Current run status when the signal was accepted or rejected.
    pub status: String,
    /// Current node ID when the signal was accepted or rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node_id: Option<String>,
}
