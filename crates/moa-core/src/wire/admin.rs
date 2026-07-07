//! Administrative maintenance wire DTOs.

use crate::*;
use serde::{Deserialize, Serialize};

/// Request payload for promoting a tenant vector backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromoteRequest {
    /// Tenant whose vector backend should be promoted.
    pub tenant_id: TenantId,
    /// Target vector backend.
    pub target_backend: String,
    /// Percentage of vectors to sample during validation.
    pub validate_percent: u32,
    /// Number of hours to dual-read both backends after cutover.
    pub dual_read_hours: u32,
}

/// Response payload describing a vector promotion or update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromotionResponse {
    /// Tenant whose vector backend was updated.
    pub tenant_id: TenantId,
    /// Number of vectors copied to the target backend.
    pub copied_vectors: u64,
    /// Average top-K overlap observed during validation.
    pub validation_overlap: f64,
    /// Active vector backend after the operation.
    pub vector_backend: String,
    /// Active vector backend state after the operation.
    pub vector_backend_state: String,
    /// Dual-read window in hours, when relevant.
    pub dual_read_hours: Option<u32>,
}

/// Request payload for rolling back or finalizing a vector promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromotionUpdateRequest {
    /// Tenant whose promotion state should be updated.
    pub tenant_id: TenantId,
    /// Promotion update action such as `rollback` or `finalize`.
    pub action: String,
}

/// Request payload for creating a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCreateRequest {
    /// Human-readable checkpoint label.
    pub label: String,
    /// Optional session associated with the checkpoint.
    pub session_id: Option<SessionId>,
}

/// Response payload for creating a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCreateResponse {
    /// Created checkpoint handle.
    pub handle: CheckpointHandle,
}

/// Response payload for listing checkpoint branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointListResponse {
    /// Active checkpoint branches ordered for API display.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointInfo>,
}

/// Request payload for rolling back to a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRollbackRequest {
    /// Neon checkpoint branch identifier.
    pub id: String,
}

/// Response payload for rolling back to a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRollbackResponse {
    /// Checkpoint selected for rollback.
    pub handle: CheckpointHandle,
}

/// Response payload for deleting expired checkpoint branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCleanupResponse {
    /// Number of expired checkpoints deleted.
    pub deleted_expired_checkpoints: u64,
}
