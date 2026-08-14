//! Durable sandbox-workspace models and lifecycle transition requests.

use chrono::{DateTime, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        contact::ContactId,
        identifiers::{
            HandProvisioningOperationId, ProviderAccountId, SandboxWorkspaceId, TenantId,
            WorkspaceCheckpointId,
        },
        sandbox_workspace::{
            DurabilityClass, SandboxWorkspaceScope, SandboxWorkspaceState, WorkspaceBinding,
        },
    },
};
use uuid::Uuid;

use super::checkpoint::archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION;
use crate::core::leases::{HandLease, LeaseHandle};

/// Exact durable identity claimed before an execution task releases its hand.
pub struct TaskHandReleaseIntent<'a> {
    /// Deterministic receipt identity.
    pub receipt_id: Uuid,
    /// Contact whose RLS scope owns the execution, when contact-scoped.
    pub contact_id: Option<ContactId>,
    /// Owning execution run.
    pub run_id: moa_core::types::identifiers::ExecutionRunScopeId,
    /// Stable task identity within the run.
    pub task_id: moa_core::types::identifiers::ExecutionTaskScopeId,
    /// Exact logical task generation being suspended.
    pub logical_generation: u64,
    /// Exact attempt generation being suspended.
    pub attempt_generation: u64,
    /// Absolute recovery deadline for this release operation.
    pub deadline_at: DateTime<Utc>,
    /// Short database claim expiry used for storage-only finalization retries.
    pub recovery_claim_expires_at: DateTime<Utc>,
    /// Exact workspace generation being checkpointed.
    pub workspace: &'a SandboxWorkspace,
    /// Exact hand generation that must be destroyed.
    pub lease: &'a HandLease,
}

/// Exact task attempt whose sandbox absence must be durably proven.
pub struct AbsentTaskHandReleaseIntent {
    /// Deterministic receipt identity.
    pub receipt_id: Uuid,
    /// Tenant owning the execution.
    pub tenant_id: TenantId,
    /// Contact whose RLS scope owns the execution, when contact-scoped.
    pub contact_id: Option<ContactId>,
    /// Owning execution run.
    pub run_id: moa_core::types::identifiers::ExecutionRunScopeId,
    /// Stable task identity.
    pub task_id: moa_core::types::identifiers::ExecutionTaskScopeId,
    /// Exact logical task generation.
    pub logical_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Time at which the database absence proof was established.
    pub verified_at: DateTime<Utc>,
}

/// Exact durable identity claimed before a compensation releases its scoped hand.
pub struct CompensationHandReleaseIntent<'a> {
    /// Deterministic receipt identity.
    pub receipt_id: Uuid,
    /// Tenant owning the execution.
    pub tenant_id: TenantId,
    /// Contact whose RLS scope owns the execution, when contact-scoped.
    pub contact_id: Option<ContactId>,
    /// Parent session whose hand scope is inspected.
    pub session_id: moa_core::types::identifiers::SessionId,
    /// Owning execution run.
    pub run_id: moa_core::types::identifiers::ExecutionRunScopeId,
    /// Stable compensation identity.
    pub compensation_id: moa_core::types::identifiers::ExecutionCompensationScopeId,
    /// Exact logical compensation generation.
    pub logical_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Opaque deterministic hand scope for this compensation.
    pub hand_scope: &'a str,
    /// Exact durable lease claimed before destroy, or `None` for verified absence.
    pub lease: Option<&'a HandLease>,
    /// Absolute recovery deadline for this release operation.
    pub deadline_at: DateTime<Utc>,
    /// Short database claim expiry used for storage-only finalization retries.
    pub recovery_claim_expires_at: DateTime<Utc>,
}

/// Exact pending compensation release whose expired storage claim is renewed.
pub struct CompensationHandReleaseClaimIntent {
    /// Tenant owning the execution.
    pub tenant_id: TenantId,
    /// Contact whose RLS scope owns the execution, when contact-scoped.
    pub contact_id: Option<ContactId>,
    /// Owning execution run.
    pub run_id: moa_core::types::identifiers::ExecutionRunScopeId,
    /// Stable compensation identity.
    pub compensation_id: moa_core::types::identifiers::ExecutionCompensationScopeId,
    /// Exact logical compensation generation.
    pub logical_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Short database claim expiry used for storage-only finalization retries.
    pub recovery_claim_expires_at: DateTime<Utc>,
}

/// Renewed recovery authority for one already-persisted compensation release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompensationHandReleaseClaim {
    /// Deterministic receipt identity selected before provider teardown.
    pub receipt_id: Uuid,
    /// Exact short-lived database claim that may finalize this receipt.
    pub claim_token: Uuid,
    /// Original release request time preserved across recovery.
    pub requested_at: DateTime<Utc>,
    /// Persisted provider create identity, absent only for a proven no-hand attempt.
    pub hand_provisioning_operation_id: Option<HandProvisioningOperationId>,
    /// Persisted lease generation paired with the provider create identity.
    pub hand_lease_generation: Option<i64>,
}

/// One durable logical workspace row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxWorkspace {
    /// Durable logical identity.
    pub workspace_id: SandboxWorkspaceId,
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Typed worker or execution-task owner.
    pub scope: SandboxWorkspaceScope,
    /// Provider selected for the pinned workspace.
    pub provider: String,
    /// Provider account and isolation cell selected for the workspace.
    pub provider_account_id: ProviderAccountId,
    /// Provider-account generation admitted at creation.
    pub provider_account_generation: i64,
    /// Required persistence behavior.
    pub durability_class: DurabilityClass,
    /// Current durable lifecycle state.
    pub state: SandboxWorkspaceState,
    /// Single-writer fencing epoch.
    pub writer_epoch: i64,
    /// Current compute-instance generation.
    pub instance_generation: i64,
    /// Current immutable checkpoint generation.
    pub checkpoint_generation: i64,
    /// Current immutable checkpoint, when one has been published.
    pub checkpoint_id: Option<WorkspaceCheckpointId>,
    /// Retention deadline, when configured.
    pub retention_deadline_at: Option<DateTime<Utc>>,
    /// Monotonic delete fence.
    pub delete_generation: i64,
    /// Time caller access was fenced for deletion.
    pub access_fenced_at: Option<DateTime<Utc>>,
}

impl SandboxWorkspace {
    /// Reconstructs the provider-neutral durable binding from this row.
    pub fn binding(&self) -> Result<WorkspaceBinding> {
        let provider_account_generation =
            u64::try_from(self.provider_account_generation).map_err(|_| {
                MoaError::StorageError(
                    "workspace provider-account generation is not positive".to_string(),
                )
            })?;
        let writer_epoch = u64::try_from(self.writer_epoch).map_err(|_| {
            MoaError::StorageError("workspace writer epoch is negative".to_string())
        })?;
        let instance_generation = u64::try_from(self.instance_generation).map_err(|_| {
            MoaError::StorageError("workspace instance generation is negative".to_string())
        })?;
        let current_revision = match (self.checkpoint_id, self.checkpoint_generation) {
            (None, 0) => None,
            (Some(checkpoint_id), generation) if generation > 0 => {
                Some(moa_core::types::sandbox_workspace::WorkspaceRevisionRef {
                    checkpoint_id,
                    generation: u64::try_from(generation).map_err(|_| {
                        MoaError::StorageError(
                            "workspace checkpoint generation is invalid".to_string(),
                        )
                    })?,
                    format_version: CHECKPOINT_ARCHIVE_FORMAT_VERSION,
                })
            }
            _ => {
                return Err(MoaError::StorageError(
                    "workspace checkpoint identity and generation disagree".to_string(),
                ));
            }
        };
        Ok(WorkspaceBinding {
            tenant_id: self.tenant_id,
            workspace_id: self.workspace_id,
            scope: self.scope.clone(),
            provider_account_id: self.provider_account_id,
            provider_account_generation,
            durability_class: self.durability_class,
            writer_epoch,
            instance_generation,
            current_revision,
        })
    }
}

/// Inputs for creating one durable workspace before provider I/O.
#[derive(Debug, Clone)]
pub struct CreateWorkspaceRequest {
    /// Replay-stable workspace identity.
    pub workspace_id: SandboxWorkspaceId,
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Verified durable execution owner.
    pub scope: SandboxWorkspaceScope,
    /// Provider selected by typed capability admission.
    pub provider: String,
    /// Selected provider account and isolation cell.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account generation.
    pub provider_account_generation: i64,
    /// Required persistence class.
    pub durability_class: DurabilityClass,
    /// Optional independent retention deadline.
    pub retention_deadline_at: Option<DateTime<Utc>>,
}

/// Secret-free provider-account binding selected for a new workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProviderAccount {
    /// Stable configured provider-account identity.
    pub provider_account_id: ProviderAccountId,
    /// Exact configured generation.
    pub generation: i64,
    /// Provider adapter name used internally by `moa-hands`.
    pub provider: String,
}

/// OpenFGA subject types allowed by the sandbox-workspace tuple matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceGrantSubjectType {
    /// Parent tenant object.
    Tenant,
    /// Parent session object.
    Session,
    /// Tenant-local contact.
    Contact,
    /// Human operator.
    Operator,
    /// Delegated or direct agent principal.
    Agent,
    /// Local API-key principal.
    ApiKey,
}

impl WorkspaceGrantSubjectType {
    /// Returns the canonical OpenFGA type label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Session => "session",
            Self::Contact => "contact",
            Self::Operator => "operator",
            Self::Agent => "agent",
            Self::ApiKey => "api_key",
        }
    }

    /// Parses one persisted OpenFGA subject-type label.
    pub(in crate::core::sandbox_workspace) fn from_label(value: &str) -> Result<Self> {
        match value {
            "tenant" => Ok(Self::Tenant),
            "session" => Ok(Self::Session),
            "contact" => Ok(Self::Contact),
            "operator" => Ok(Self::Operator),
            "agent" => Ok(Self::Agent),
            "api_key" => Ok(Self::ApiKey),
            other => Err(MoaError::StorageError(format!(
                "unknown workspace grant subject type: {other}"
            ))),
        }
    }
}

/// Relations persisted in the exact sandbox-workspace grant ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceGrantRelation {
    /// Parent tenant edge.
    Tenant,
    /// Parent session edge.
    Session,
    /// Direct resource owner.
    Owner,
    /// Administrative workspace control.
    Manage,
    /// Workspace filesystem use.
    Use,
}

impl WorkspaceGrantRelation {
    /// Returns the canonical OpenFGA relation label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Session => "session",
            Self::Owner => "owner",
            Self::Manage => "manage",
            Self::Use => "use",
        }
    }

    /// Parses one persisted workspace-grant relation label.
    pub(in crate::core::sandbox_workspace) fn from_label(value: &str) -> Result<Self> {
        match value {
            "tenant" => Ok(Self::Tenant),
            "session" => Ok(Self::Session),
            "owner" => Ok(Self::Owner),
            "manage" => Ok(Self::Manage),
            "use" => Ok(Self::Use),
            other => Err(MoaError::StorageError(format!(
                "unknown workspace grant relation: {other}"
            ))),
        }
    }
}

/// One exact desired OpenFGA tuple for a sandbox workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGrant {
    /// Stable ledger row identity.
    pub grant_id: Uuid,
    /// Typed tuple subject.
    pub subject_type: WorkspaceGrantSubjectType,
    /// Subject identifier.
    pub subject_id: Uuid,
    /// Optional userset relation for future model versions.
    pub subject_relation: Option<String>,
    /// Workspace relation granted to the subject.
    pub relation: WorkspaceGrantRelation,
}

impl WorkspaceGrant {
    /// Renders the exact OpenFGA subject or userset string.
    #[must_use]
    pub fn subject_wire(&self) -> String {
        match self.subject_relation.as_deref() {
            Some(relation) => format!(
                "{}:{}#{relation}",
                self.subject_type.as_str(),
                self.subject_id
            ),
            None => format!("{}:{}", self.subject_type.as_str(), self.subject_id),
        }
    }
}

/// Exact compare-and-set state transition.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceTransition {
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Workspace being transitioned.
    pub workspace_id: SandboxWorkspaceId,
    /// Required current state.
    pub from: SandboxWorkspaceState,
    /// Requested next state.
    pub to: SandboxWorkspaceState,
    /// Required writer fence.
    pub writer_epoch: i64,
    /// Required compute-instance fence.
    pub instance_generation: i64,
}

/// Request to claim the one writable attachment for a workspace.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceWriterClaim {
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Workspace receiving a writer.
    pub workspace_id: SandboxWorkspaceId,
    /// Required detached state.
    pub expected_state: SandboxWorkspaceState,
    /// Required prior writer fence.
    pub expected_writer_epoch: i64,
    /// Required prior instance fence.
    pub expected_instance_generation: i64,
}

/// Exact provisioning lease and workspace inputs for hydrated activation.
#[derive(Debug)]
pub struct ActivateHydratedWorkspaceRequest<'a> {
    /// Workspace ownership, head, and generation fences used for hydration.
    pub binding: &'a WorkspaceBinding,
    /// Exact provisioning lease generation that received the hydrated hand.
    pub lease: &'a HandLease,
    /// Durable handle payload published only with workspace activation.
    pub handle: LeaseHandle,
}
