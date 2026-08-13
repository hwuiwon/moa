//! Durable sandbox-workspace contracts shared across providers and persistence owners.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{MoaError, Result};
use crate::types::hands::HandHandle;
use crate::types::identifiers::{
    ExecutionCompensationScopeId, ExecutionRunScopeId, ExecutionTaskScopeId,
    HandProvisioningOperationId, ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId,
    WorkspaceCheckpointId, WorkspaceOperationId,
};
use crate::types::worker::state::WorkerId;

/// The durable execution owner of one sandbox workspace.
///
/// Bare session/coordinator workspaces are intentionally unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxWorkspaceScope {
    /// Filesystem state owned by one conversational worker.
    Worker {
        /// Durable parent session.
        session_id: SessionId,
        /// Worker identity within the session.
        worker_id: WorkerId,
    },
    /// Filesystem state owned by one durable execution task.
    ExecutionTask {
        /// Boundary reference constructed from a verified execution run ID.
        run_id: ExecutionRunScopeId,
        /// Boundary reference constructed from a verified execution task ID.
        task_id: ExecutionTaskScopeId,
    },
}

/// Required durability contract for a sandbox workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurabilityClass {
    /// Filesystem-only state committed to provider-independent portable checkpoints.
    PortableFilesystem,
}

/// Whether a sandbox tool can change the durable workspace filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEffect {
    /// The tool observes files without changing the workspace revision.
    ReadOnly,
    /// The tool may change files and therefore requires the commit barrier.
    MayWrite,
}

/// Immutable reference to one committed workspace revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRevisionRef {
    /// Immutable checkpoint that contains this revision.
    pub checkpoint_id: WorkspaceCheckpointId,
    /// Monotonic committed generation within the logical workspace.
    pub generation: u64,
    /// Portable checkpoint format version used by this revision.
    pub format_version: u16,
}

/// Ownership and fencing data attached to every persistent hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Typed durable execution owner.
    pub scope: SandboxWorkspaceScope,
    /// Durable logical workspace identity.
    pub workspace_id: SandboxWorkspaceId,
    /// Configured provider account and isolation cell selected for the workspace.
    pub provider_account_id: ProviderAccountId,
    /// Persisted provider-account generation selected for this workspace.
    pub provider_account_generation: u64,
    /// Required persistence contract.
    pub durability_class: DurabilityClass,
    /// Current single-writer fencing epoch.
    pub writer_epoch: u64,
    /// Current provider compute-instance generation.
    pub instance_generation: u64,
    /// Current verified committed revision restored into the hand.
    ///
    /// `None` is valid only while the durable workspace head is generation zero.
    /// Lifecycle code must reject it after any checkpoint has been published.
    pub current_revision: Option<WorkspaceRevisionRef>,
}

/// Durable lifecycle state of a logical sandbox workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxWorkspaceState {
    /// Metadata and initial storage are being created.
    Creating,
    /// Durable state exists without a writable compute attachment.
    Ready,
    /// The current fenced writer may dispatch tools.
    Active,
    /// New dispatch is blocked while in-flight writers stop.
    Quiescing,
    /// A new immutable revision is being published.
    Committing,
    /// A verified revision is being restored into fresh compute.
    Restoring,
    /// An ambiguous external outcome requires durable reconciliation.
    Reconciling,
    /// Working state failed while the prior committed revision remains authoritative.
    Failed,
    /// Access is fenced while external state is removed.
    Deleting,
    /// External and relational workspace state has been deleted.
    Deleted,
}

/// Durable lifecycle state of an immutable workspace checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCheckpointState {
    /// Checkpoint bytes are being created and verified.
    Creating,
    /// The complete immutable checkpoint is available for restore.
    Available,
    /// Checkpoint deletion has been fenced and claimed.
    Deleting,
    /// Checkpoint bytes and key material have been deleted.
    Deleted,
    /// Publication failed and this checkpoint can never become current.
    Failed,
}

/// External storage operation recorded before provider I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOperationKind {
    /// Create durable provider storage.
    Create,
    /// Attach a workspace to writable compute.
    Attach,
    /// Commit the current working filesystem.
    Commit,
    /// Create an immutable checkpoint.
    Checkpoint,
    /// Restore a committed revision.
    Restore,
    /// Delete provider storage.
    Delete,
}

/// Durable classification of an external provider operation outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOperationOutcome {
    /// The durable intent exists but no provider request was sent.
    NotSent,
    /// A request may have reached the provider and must be reconciled.
    Unknown,
    /// The provider result was verified against the durable intent.
    Confirmed,
}

/// Verified provider-resource disposition for a confirmed operation.
///
/// This is separate from [`WorkspaceOperationOutcome`] so a confirmed
/// no-create result remains distinguishable from a confirmed existing
/// resource across retries and process restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceConfirmedDisposition {
    /// The exact fenced provider resource is present.
    ResourcePresent,
    /// The exact fenced provider resource is absent.
    ResourceAbsent,
}

/// Provider-neutral category of opaque durable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStorageKind {
    /// Mutable filesystem working state.
    MutableFilesystem,
    /// Provider-independent portable checkpoint authority.
    PortableCheckpoint,
}

/// Opaque internal reference to provider-owned workspace storage.
///
/// The opaque fields are persistence-internal and must never be exposed as an
/// authorization token or copied into public APIs, events, prompts, or labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderStorageRef {
    /// Provider account that owns the resource.
    pub provider_account_id: ProviderAccountId,
    /// Provider-account generation that owns the resource.
    pub provider_account_generation: u64,
    /// Semantic category of the referenced storage.
    pub kind: ProviderStorageKind,
    /// Provider-specific opaque resource identifier.
    pub resource_id: String,
    /// Optional provider-specific opaque locator within the resource.
    pub workspace_locator: Option<String>,
}

/// Capacity dimension admitted independently for a provider account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCapacityDimension {
    /// Logical workspaces.
    Workspaces,
    /// Ephemeral sandbox compute instances with a live durable owner.
    ActiveHands,
    /// Provider volumes.
    Volumes,
    /// Immutable checkpoint count.
    Checkpoints,
    /// Total logical uncompressed checkpoint bytes.
    LogicalBytes,
}

/// Exact durable execution owner whose bounded attempt released sandbox compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionHandReleaseOwner {
    /// Forward task owner with its exact logical generation.
    Task {
        /// Stable execution task.
        task_id: ExecutionTaskScopeId,
        /// Exact logical task generation.
        logical_generation: u64,
    },
    /// Rollback compensation owner with its exact logical generation.
    Compensation {
        /// Stable compensation registration.
        compensation_id: ExecutionCompensationScopeId,
        /// Exact logical compensation generation.
        logical_generation: u64,
    },
}

/// Durable proof that one exact execution attempt released its sandbox compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionHandReleaseReceipt {
    /// Deterministic receipt identity.
    pub receipt_id: uuid::Uuid,
    /// Tenant owner.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_id: ExecutionRunScopeId,
    /// Exact task or compensation owner and logical generation.
    pub owner: ExecutionHandReleaseOwner,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Released workspace, present for task-owned durable filesystems.
    pub workspace_id: Option<SandboxWorkspaceId>,
    /// Exact writer generation checkpointed by the attempt.
    pub writer_epoch: Option<u64>,
    /// Exact compute instance generation destroyed by the attempt.
    pub instance_generation: Option<u64>,
    /// Provider-visible hand creation identity that was destroyed.
    pub hand_provisioning_operation_id: Option<HandProvisioningOperationId>,
    /// Exact durable hand lease generation that was released.
    pub hand_lease_generation: Option<u64>,
    /// Verified portable checkpoint promoted as recovery authority.
    pub checkpoint_id: Option<WorkspaceCheckpointId>,
    /// Monotonic checkpoint generation.
    pub checkpoint_generation: Option<u64>,
    /// Verified canonical checkpoint manifest digest.
    pub checkpoint_manifest_digest: Option<String>,
    /// Exact logical bytes charged to the checkpoint.
    pub checkpoint_logical_bytes: Option<u64>,
    /// Time the release operation was first requested.
    pub requested_at: DateTime<Utc>,
    /// Time verified provider absence and durable release completed.
    pub released_at: DateTime<Utc>,
}

macro_rules! impl_persisted_workspace_labels {
    ($type:ty, $kind:literal, {$($variant:path => $label:literal),+ $(,)?}) => {
        impl $type {
            /// Returns the stable database and telemetry label.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $($variant => $label),+
                }
            }

            /// Parses a stable persisted label and rejects unknown values.
            pub fn from_label(value: &str) -> Result<Self> {
                match value {
                    $($label => Ok($variant)),+,
                    other => Err(MoaError::ValidationError(format!(
                        "unknown {}: {other}",
                        $kind
                    ))),
                }
            }
        }
    };
}

impl_persisted_workspace_labels!(DurabilityClass, "workspace durability class", {
    DurabilityClass::PortableFilesystem => "portable_filesystem",
});
impl_persisted_workspace_labels!(WorkspaceEffect, "workspace effect", {
    WorkspaceEffect::ReadOnly => "read_only",
    WorkspaceEffect::MayWrite => "may_write",
});
impl_persisted_workspace_labels!(SandboxWorkspaceState, "sandbox workspace state", {
    SandboxWorkspaceState::Creating => "creating",
    SandboxWorkspaceState::Ready => "ready",
    SandboxWorkspaceState::Active => "active",
    SandboxWorkspaceState::Quiescing => "quiescing",
    SandboxWorkspaceState::Committing => "committing",
    SandboxWorkspaceState::Restoring => "restoring",
    SandboxWorkspaceState::Reconciling => "reconciling",
    SandboxWorkspaceState::Failed => "failed",
    SandboxWorkspaceState::Deleting => "deleting",
    SandboxWorkspaceState::Deleted => "deleted",
});
impl_persisted_workspace_labels!(WorkspaceCheckpointState, "workspace checkpoint state", {
    WorkspaceCheckpointState::Creating => "creating",
    WorkspaceCheckpointState::Available => "available",
    WorkspaceCheckpointState::Deleting => "deleting",
    WorkspaceCheckpointState::Deleted => "deleted",
    WorkspaceCheckpointState::Failed => "failed",
});
impl_persisted_workspace_labels!(WorkspaceOperationKind, "workspace operation kind", {
    WorkspaceOperationKind::Create => "create",
    WorkspaceOperationKind::Attach => "attach",
    WorkspaceOperationKind::Commit => "commit",
    WorkspaceOperationKind::Checkpoint => "checkpoint",
    WorkspaceOperationKind::Restore => "restore",
    WorkspaceOperationKind::Delete => "delete",
});
impl_persisted_workspace_labels!(WorkspaceOperationOutcome, "workspace operation outcome", {
    WorkspaceOperationOutcome::NotSent => "not_sent",
    WorkspaceOperationOutcome::Unknown => "unknown",
    WorkspaceOperationOutcome::Confirmed => "confirmed",
});
impl_persisted_workspace_labels!(WorkspaceConfirmedDisposition, "workspace confirmed disposition", {
    WorkspaceConfirmedDisposition::ResourcePresent => "resource_present",
    WorkspaceConfirmedDisposition::ResourceAbsent => "resource_absent",
});
impl_persisted_workspace_labels!(ProviderStorageKind, "provider storage kind", {
    ProviderStorageKind::MutableFilesystem => "mutable_filesystem",
    ProviderStorageKind::PortableCheckpoint => "portable_checkpoint",
});
impl_persisted_workspace_labels!(WorkspaceCapacityDimension, "workspace capacity dimension", {
    WorkspaceCapacityDimension::Workspaces => "workspaces",
    WorkspaceCapacityDimension::ActiveHands => "active_hands",
    WorkspaceCapacityDimension::Volumes => "volumes",
    WorkspaceCapacityDimension::Checkpoints => "checkpoints",
    WorkspaceCapacityDimension::LogicalBytes => "logical_bytes",
});

/// Maintenance-visible provider resource categories.
///
/// This enum is intentionally closed and low-cardinality. Provider-specific
/// resource identifiers remain opaque internal evidence and never become
/// public API fields or metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderInventoryResourceKind {
    /// Ephemeral sandbox compute.
    Compute,
    /// Mutable provider filesystem storage such as a Daytona volume.
    MutableFilesystem,
}

/// Provider-verified MOA ownership metadata attached to one inventory item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInventoryOwner {
    /// Tenant named by authenticated provider metadata.
    pub tenant_id: TenantId,
    /// Durable workspace named by authenticated provider metadata.
    pub workspace_id: SandboxWorkspaceId,
    /// Exact durable hand-provisioning intent, for compute inventory.
    pub provisioning_operation_id: Option<HandProvisioningOperationId>,
    /// Writer epoch carried by the provider resource, when applicable.
    pub writer_epoch: Option<u64>,
    /// Compute-instance generation carried by the resource, when applicable.
    pub instance_generation: Option<u64>,
}

/// One provider resource returned only to the maintenance coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderInventoryResource {
    /// Closed resource category.
    pub kind: ProviderInventoryResourceKind,
    /// Opaque provider identifier used only for exact durable-row comparison.
    pub provider_reference: String,
    /// Stable non-reversible fingerprint persisted in the finding ledger.
    pub resource_fingerprint: String,
    /// Digest of the complete verified evidence used for this observation.
    pub evidence_digest: String,
    /// Authenticated MOA ownership metadata, absent for unknown resources.
    pub verified_owner: Option<ProviderInventoryOwner>,
}

/// Complete bounded inventory for one persisted provider-account generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccountStorageInventory {
    /// Exact configured provider account.
    pub provider_account_id: ProviderAccountId,
    /// Exact account generation used for the authenticated request.
    pub provider_account_generation: u64,
    /// Provider observation time.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// Complete provider inventory in stable fingerprint order.
    pub resources: Vec<ProviderInventoryResource>,
}

/// Exact tenant-wide storage resource selected by the purge coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantStoragePurgeRequest {
    /// Tenant whose global destruction fence is active.
    pub tenant_id: TenantId,
    /// Stable tenant purge operation identity.
    pub purge_operation_id: String,
    /// Exact provider-account generation being reconciled.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account generation being reconciled.
    pub provider_account_generation: u64,
    /// Exact tenant-owned provider storage reference from the durable row.
    pub storage: ProviderStorageRef,
}

/// Common durable operation context supplied to provider storage calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStorageOperation {
    /// Durable operation identity persisted before provider I/O.
    pub operation_id: WorkspaceOperationId,
    /// Operation being performed.
    pub kind: WorkspaceOperationKind,
    /// Current ownership and generation fences.
    pub binding: WorkspaceBinding,
    /// Absolute operation deadline.
    pub deadline: chrono::DateTime<chrono::Utc>,
    /// Canonical request hash persisted with the intent.
    pub request_hash: String,
}

/// Request to create or resolve durable mutable storage before compute exists.
///
/// Providers such as Daytona can mount volumes only in their sandbox-create
/// request, so lifecycle code must durably prepare storage before provisioning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStoragePrepareRequest {
    /// Exact create operation persisted before provider I/O.
    pub operation: WorkspaceStorageOperation,
}

/// Request to attach mutable workspace storage to compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAttachRequest {
    /// Durable operation context.
    pub operation: WorkspaceStorageOperation,
    /// Compute instance receiving the writable attachment.
    pub hand: HandHandle,
    /// Existing provider storage, or `None` when the provider must create it.
    pub storage: Option<ProviderStorageRef>,
}

/// Request to publish quiesced working state as a new immutable checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCheckpointPublishRequest {
    /// Durable commit or checkpoint operation context.
    pub operation: WorkspaceStorageOperation,
    /// Quiesced compute instance containing the mutable data root.
    pub hand: HandHandle,
    /// Parent committed revision being advanced, absent only at generation zero.
    pub parent_revision: Option<WorkspaceRevisionRef>,
    /// Whether verified publication must destroy compute before reporting success.
    pub release_compute: bool,
}

/// Request to restore one verified checkpoint into compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestoreRequest {
    /// Durable operation context.
    pub operation: WorkspaceStorageOperation,
    /// Fresh compute instance receiving the restore.
    pub hand: HandHandle,
    /// Verified committed revision to restore.
    pub revision: WorkspaceRevisionRef,
    /// Opaque checkpoint storage reference.
    pub checkpoint: ProviderStorageRef,
}

/// Request to delete provider-owned workspace storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStorageDeleteRequest {
    /// Durable operation context.
    pub operation: WorkspaceStorageOperation,
    /// Exact fenced provider storage being deleted.
    pub storage: ProviderStorageRef,
}

/// Request to reconcile an operation whose external outcome is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceReconcileRequest {
    /// Exact durable operation context reconstructed from the operation ledger.
    operation: WorkspaceStorageOperation,
    /// Exact compute resource learned from a verified provider result, when any.
    hand: Option<HandHandle>,
    /// Exact storage resource learned from a durable row or verified result.
    storage: Option<ProviderStorageRef>,
}

impl WorkspaceReconcileRequest {
    /// Builds an exact-resource reconciliation request from durable typed state.
    ///
    /// Provider account identity is derived from the persisted workspace binding;
    /// callers cannot supply a second raw account selector that could disagree.
    pub fn new(
        operation: WorkspaceStorageOperation,
        hand: Option<HandHandle>,
        storage: Option<ProviderStorageRef>,
    ) -> Result<Self> {
        let request = Self {
            operation,
            hand,
            storage,
        };
        request.validate()?;
        Ok(request)
    }

    /// Revalidates all exact-resource fences after a durable or wire round trip.
    pub fn validate(&self) -> Result<()> {
        let operation = &self.operation;
        if operation.request_hash.trim().is_empty() {
            return Err(MoaError::ValidationError(
                "reconciliation operation does not match its workspace account fence".to_string(),
            ));
        }
        if let Some((account_id, generation)) =
            self.hand.as_ref().and_then(HandHandle::provider_account)
            && (account_id != operation.binding.provider_account_id
                || generation != operation.binding.provider_account_generation)
        {
            return Err(MoaError::ValidationError(
                "reconciliation hand does not match its workspace account fence".to_string(),
            ));
        }
        if self.storage.as_ref().is_some_and(|storage| {
            storage.provider_account_id != operation.binding.provider_account_id
                || storage.provider_account_generation
                    != operation.binding.provider_account_generation
        }) {
            return Err(MoaError::ValidationError(
                "reconciliation storage does not match its workspace account fence".to_string(),
            ));
        }
        Ok(())
    }

    /// Returns the exact persisted operation context.
    #[must_use]
    pub const fn operation(&self) -> &WorkspaceStorageOperation {
        &self.operation
    }

    /// Returns the exact verified compute handle, when reconciliation concerns one.
    #[must_use]
    pub const fn hand(&self) -> Option<&HandHandle> {
        self.hand.as_ref()
    }

    /// Returns the exact durable storage reference, when reconciliation concerns one.
    #[must_use]
    pub const fn storage(&self) -> Option<&ProviderStorageRef> {
        self.storage.as_ref()
    }
}

/// Verified result of one provider storage operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceStorageOperationResult {
    /// Whether the external outcome is confirmed or still ambiguous.
    pub outcome: WorkspaceOperationOutcome,
    /// Verified resource disposition, present exactly for confirmed outcomes.
    pub confirmed_disposition: Option<WorkspaceConfirmedDisposition>,
    /// Opaque storage created or resolved by the provider, when applicable.
    pub storage: Option<ProviderStorageRef>,
    /// Complete verified checkpoint publication, present for successful commit
    /// and checkpoint operations.
    pub checkpoint_publication: Option<WorkspaceCheckpointPublication>,
    /// Provider-selected compute disposition after a successful commit.
    pub post_commit_state: Option<WorkspacePostCommitState>,
}

/// Complete verified result of publishing one portable checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCheckpointPublication {
    /// Exact immutable workspace revision created by the operation.
    pub revision: WorkspaceRevisionRef,
    /// Opaque portable checkpoint storage reference.
    pub storage: ProviderStorageRef,
    /// SHA-256 digest of the canonical final manifest.
    pub manifest_digest: String,
    /// Logical uncompressed bytes represented by the checkpoint.
    pub logical_bytes: u64,
}

/// Provider-selected compute disposition after a verified commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePostCommitState {
    /// The exact writable attachment and compute instance remain usable.
    AttachmentRetained,
    /// The provider destroyed compute after publishing the checkpoint.
    ComputeDestroyed,
}

/// Non-overlapping filesystem roots inside one sandbox.
///
/// Only `mutable_root` may be mounted, exported, or restored by workspace
/// persistence. Trusted material and runtime controls are rebuilt from current
/// authority after hydration and must remain outside checkpoint bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxFilesystemLayout {
    /// Tenant-created filesystem state eligible for checkpointing.
    pub mutable_root: PathBuf,
    /// Current trusted files installed by MOA after hydration.
    pub trusted_root: PathBuf,
    /// Runtime controls, credentials, tokens, and policy material.
    pub runtime_root: PathBuf,
}

impl SandboxFilesystemLayout {
    /// Returns MOA's fixed provider-neutral sandbox root layout.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            mutable_root: PathBuf::from("/workspace"),
            trusted_root: PathBuf::from("/opt/moa/trusted"),
            runtime_root: PathBuf::from("/run/moa"),
        }
    }

    /// Verifies that all roots are absolute and pairwise disjoint.
    pub fn validate(&self) -> Result<()> {
        let roots = [&self.mutable_root, &self.trusted_root, &self.runtime_root];
        if roots.iter().any(|root| !root.is_absolute()) {
            return Err(MoaError::ValidationError(
                "sandbox filesystem roots must be absolute".to_string(),
            ));
        }
        for (index, left) in roots.iter().enumerate() {
            for right in roots.iter().skip(index + 1) {
                if left == right || left.starts_with(right) || right.starts_with(left) {
                    return Err(MoaError::ValidationError(
                        "sandbox mutable, trusted, and runtime roots must be disjoint".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}
