//! Procedure service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for starting a skill-backed procedure run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureRunRequest {
    /// Tenant used for authorization and execution.
    pub tenant_id: TenantId,
    /// Skill artifact reference carrying the procedure, for example `skill://damaged-food-order`.
    pub procedure_ref: String,
    /// Initial procedure input.
    #[serde(default)]
    pub input: Value,
    /// Optional session that should receive agent-loop work.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Optional idempotency key for run creation.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// Response payload returned when a procedure run is started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureRunResponse {
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Initial run status.
    pub status: String,
}

/// Request payload for loading procedure run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureStatusRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Procedure run row identifier.
    pub run_id: Uuid,
}

/// Response payload for procedure run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureRunStatus {
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Session associated with this procedure run, when present.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Current node ID, if execution has started.
    pub current_node_id: Option<String>,
    /// Current run status.
    pub status: String,
    /// Per-node run summaries.
    #[serde(default)]
    pub node_runs: Vec<ProcedureNodeRunSummary>,
    /// Terminal output payload.
    #[serde(default)]
    pub output: Option<Value>,
    /// Terminal error text.
    #[serde(default)]
    pub error: Option<String>,
}

/// Summary of one procedure node execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureNodeRunSummary {
    /// Procedure node ID.
    pub node_id: String,
    /// Node run status.
    pub status: String,
    /// Node start timestamp.
    pub started_at: DateTime<Utc>,
    /// Node completion timestamp.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Request payload for cancelling a procedure run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureCancelRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Optional cancellation reason.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response payload returned after requesting procedure cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureCancelResponse {
    /// Whether cancellation was accepted.
    pub cancelled: bool,
    /// Human-readable status message.
    pub reason: String,
}

/// Decision kind for a procedure review node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcedureReviewDecisionKind {
    /// Approve the waiting procedure review node.
    Approved,
    /// Reject the waiting procedure review node.
    Rejected,
}

/// Request payload for deciding a procedure review node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureReviewDecisionRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Review node to decide. Defaults to the run's current node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Decision to apply.
    pub decision: ProcedureReviewDecisionKind,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional approved output to store under the review node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// Response payload returned after deciding a procedure review node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureReviewDecisionResponse {
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Whether this request changed procedure state.
    pub accepted: bool,
    /// Current run status when the decision was accepted or rejected.
    pub status: String,
    /// Current node ID after the decision was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node_id: Option<String>,
}

/// Request payload for delivering an external procedure signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureSignalRequest {
    /// Tenant used for authorization.
    pub tenant_id: TenantId,
    /// Procedure run row identifier.
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

/// Response payload returned after delivering an external procedure signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcedureSignalResponse {
    /// Procedure run row identifier.
    pub run_id: Uuid,
    /// Whether this request was accepted by the waiting procedure.
    pub accepted: bool,
    /// Current run status when the signal was accepted or rejected.
    pub status: String,
    /// Current node ID when the signal was accepted or rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_node_id: Option<String>,
}
