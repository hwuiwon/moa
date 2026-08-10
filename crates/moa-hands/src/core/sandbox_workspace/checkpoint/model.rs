//! Durable checkpoint metadata and atomic publication requests.

use chrono::{DateTime, Utc};
use moa_core::types::{
    identifiers::{SandboxWorkspaceId, TenantId, WorkspaceCheckpointId, WorkspaceOperationId},
    sandbox_workspace::{
        WorkspaceBinding, WorkspaceCheckpointPublication, WorkspaceCheckpointState,
        WorkspacePostCommitState,
    },
};

use crate::core::leases::HandLease;

/// Exact verified checkpoint result and lease disposition committed atomically.
#[derive(Debug)]
pub struct PublishCheckpointCommitRequest<'a> {
    /// Workspace binding used by the provider operation.
    pub binding: &'a WorkspaceBinding,
    /// Exact durable commit operation that owns the checkpoint row.
    pub operation_id: WorkspaceOperationId,
    /// Provider result whose complete fields must match the operation fences.
    pub publication: &'a WorkspaceCheckpointPublication,
    /// Provider-selected compute disposition after checkpoint publication.
    pub post_commit_state: WorkspacePostCommitState,
    /// Exact active lease generation that held the committed writer.
    pub lease: &'a HandLease,
}

/// Immutable checkpoint metadata and its byte-publication lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCheckpoint {
    /// Immutable checkpoint identity.
    pub checkpoint_id: WorkspaceCheckpointId,
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Owning logical workspace.
    pub workspace_id: SandboxWorkspaceId,
    /// Monotonic revision generation.
    pub generation: i64,
    /// Exact parent revision, absent only for generation one.
    pub parent_checkpoint_id: Option<WorkspaceCheckpointId>,
    /// Writer fence captured before byte I/O.
    pub source_writer_epoch: i64,
    /// Compute-instance fence captured before byte I/O.
    pub source_instance_generation: i64,
    /// Operation intent that owns this publication.
    pub operation_id: WorkspaceOperationId,
    /// Current immutable-checkpoint lifecycle state.
    pub state: WorkspaceCheckpointState,
    /// Portable object-store reference, present only after verification.
    pub object_reference: Option<String>,
    /// Verified canonical manifest digest.
    pub manifest_digest: Option<String>,
    /// Verified logical byte count.
    pub logical_bytes: Option<i64>,
    /// Time the complete payload was verified.
    pub verified_at: Option<DateTime<Utc>>,
}

/// Intent-first request for one immutable checkpoint row.
#[derive(Debug, Clone, Copy)]
pub struct CreateCheckpointRequest {
    /// Replay-stable checkpoint identity.
    pub checkpoint_id: WorkspaceCheckpointId,
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Owning workspace.
    pub workspace_id: SandboxWorkspaceId,
    /// Exact parent revision, absent only for generation one.
    pub parent_checkpoint_id: Option<WorkspaceCheckpointId>,
    /// Checkpoint provider operation persisted before this row.
    pub operation_id: WorkspaceOperationId,
    /// Required writer fence.
    pub expected_writer_epoch: i64,
    /// Required compute-instance fence.
    pub expected_instance_generation: i64,
    /// Required current workspace head generation.
    pub expected_checkpoint_generation: i64,
}
