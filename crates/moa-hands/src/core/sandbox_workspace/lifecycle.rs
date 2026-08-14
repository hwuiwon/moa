//! Managed durable sandbox-workspace lifecycle and commit barriers.

mod commit;
mod execution_release;
mod management;
mod materialization;
mod worker_release;

use chrono::{Duration as ChronoDuration, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        hands::HandHandle,
        identifiers::{
            ExecutionCompensationScopeId, SandboxWorkspaceId, ToolCallId, WorkspaceCheckpointId,
            WorkspaceOperationId,
        },
        sandbox_workspace::{
            ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt, ProviderStorageKind,
            ProviderStorageRef, SandboxWorkspaceScope, SandboxWorkspaceState,
            WorkspaceAttachRequest, WorkspaceBinding, WorkspaceCheckpointPublishRequest,
            WorkspaceCheckpointState, WorkspaceConfirmedDisposition, WorkspaceOperationKind,
            WorkspaceOperationOutcome, WorkspacePostCommitState, WorkspaceReconcileRequest,
            WorkspaceRestoreRequest, WorkspaceStorageOperation, WorkspaceStoragePrepareRequest,
        },
        session::SessionMeta,
    },
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    checkpoint::{
        archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION,
        model::{CreateCheckpointRequest, PublishCheckpointCommitRequest},
    },
    failpoints,
    model::{
        AbsentTaskHandReleaseIntent, CompensationHandReleaseClaimIntent,
        CompensationHandReleaseIntent, SandboxWorkspace, TaskHandReleaseIntent,
        WorkspaceTransition, WorkspaceWriterClaim,
    },
    operations::WorkspaceOperationIntent,
};
use moa_observability::{
    SandboxWorkspaceCheckpointOperation, SandboxWorkspaceLifecycleOperation,
    SandboxWorkspaceMetricResult,
};

use crate::core::{
    ActiveHand, ExecutionHandReleaseRequest, HandProviderCacheKey, HandRoute,
    InstalledManifestMarker, JournaledWorkspaceCommit, ToolCallScope, ToolExecution, ToolRouter,
    TrustedSandboxManifest,
    leases::{HandLease, HandLeaseStatus, HandLeaseWorkspaceAttachment},
    lifecycle::{
        manifest_scope_key, session_provider_key, workspace_binding_for_hand, workspace_lease_scope,
    },
    telemetry::{
        record_workspace_checkpoint, record_workspace_lifecycle, record_workspace_release,
        record_workspace_restore,
    },
};

#[derive(Clone, Copy)]
/// Internal identity and policy for one deterministic workspace commit.
pub(in crate::core) struct WorkspaceCommitExecution<'a> {
    /// Session owning the workspace.
    pub(in crate::core) session: &'a SessionMeta,
    /// Typed durable workspace owner.
    pub(in crate::core) workspace_scope: &'a SandboxWorkspaceScope,
    /// Deterministic tool or yield identity.
    pub(in crate::core) tool_call_id: ToolCallId,
    /// Pinned hand and storage provider.
    pub(in crate::core) provider_name: &'a str,
    /// Exact active compute handle.
    pub(in crate::core) hand: &'a HandHandle,
    /// Bounded execution scope.
    pub(in crate::core) call_scope: ToolCallScope<'a>,
    /// Whether verified publication must destroy compute.
    pub(in crate::core) release_compute: bool,
}

impl ToolRouter {
    async fn delete_abandoned_checkpoint_prefix(
        &self,
        binding: &WorkspaceBinding,
        checkpoint_id: WorkspaceCheckpointId,
    ) -> Result<()> {
        let store = self.hands.checkpoint_store.as_ref().ok_or_else(|| {
            MoaError::ConfigError(
                "checkpoint CAS cleanup requires the durable checkpoint store".to_string(),
            )
        })?;
        store
            .delete(
                crate::core::sandbox_workspace::checkpoint::store::CheckpointStoreContext {
                    tenant_id: binding.tenant_id,
                    workspace_id: binding.workspace_id,
                    checkpoint_id,
                    provider_account_id: binding.provider_account_id,
                    provider_account_generation: binding.provider_account_generation,
                },
            )
            .await
    }
}

pub(in crate::core) fn validate_managed_restore_target(
    current_checkpoint_id: Option<WorkspaceCheckpointId>,
    current_generation: i64,
    requested_checkpoint_id: WorkspaceCheckpointId,
    checkpoint_id: WorkspaceCheckpointId,
    checkpoint_generation: i64,
    checkpoint_state: WorkspaceCheckpointState,
) -> Result<()> {
    if checkpoint_state != WorkspaceCheckpointState::Available
        || checkpoint_id != requested_checkpoint_id
        || current_checkpoint_id != Some(requested_checkpoint_id)
        || current_generation != checkpoint_generation
    {
        return Err(MoaError::ValidationError(
            "restore requires the exact available current workspace checkpoint".to_string(),
        ));
    }
    Ok(())
}

pub(in crate::core) fn lease_attachment(
    binding: &WorkspaceBinding,
) -> Result<HandLeaseWorkspaceAttachment> {
    HandLeaseWorkspaceAttachment::new(
        binding.workspace_id,
        i64::try_from(binding.writer_epoch).map_err(|_| {
            MoaError::ValidationError("workspace writer epoch overflows bigint".to_string())
        })?,
        i64::try_from(binding.instance_generation).map_err(|_| {
            MoaError::ValidationError("workspace instance generation overflows bigint".to_string())
        })?,
        binding
            .current_revision
            .as_ref()
            .map(|revision| revision.checkpoint_id),
    )
}
