//! Tenant-scoped durable sandbox-workspace repository and state machine.

mod base;
mod checkpoints;
mod grants;
mod lifecycle;

use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{
            ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId, SandboxWorkspaceId,
            SessionId, TenantId, WorkspaceCheckpointId, WorkspaceOperationId,
        },
        memory::RlsContext,
        sandbox_workspace::{
            DurabilityClass, ProviderStorageKind, SandboxWorkspaceScope, SandboxWorkspaceState,
            WorkspaceBinding, WorkspaceCheckpointPublication, WorkspaceCheckpointState,
            WorkspaceOperationKind, WorkspacePostCommitState,
        },
    },
};
use moa_db::ScopedConn;
use sqlx::{PgConnection, PgPool, Row, types::Json};
use uuid::Uuid;

use super::{
    checkpoint::{
        archive::CHECKPOINT_ARCHIVE_FORMAT_VERSION,
        model::{CreateCheckpointRequest, PublishCheckpointCommitRequest, WorkspaceCheckpoint},
    },
    failpoints,
    model::{
        ActivateHydratedWorkspaceRequest, CreateWorkspaceRequest, SandboxWorkspace, WorkspaceGrant,
        WorkspaceGrantRelation, WorkspaceGrantSubjectType, WorkspaceProviderAccount,
        WorkspaceTransition, WorkspaceWriterClaim,
    },
    operations::ClaimedWorkspaceOperation,
};
use crate::core::leases::{HandLease, HandLeaseStatus, map_sqlx_error};

/// Postgres-backed tenant workspace repository.
#[derive(Clone)]
pub struct PostgresWorkspaceRepository {
    pool: PgPool,
    assume_workspace_maintenance_role: bool,
}

impl PostgresWorkspaceRepository {
    /// Creates a repository over the runtime Postgres pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: false,
        }
    }

    /// Creates a repository over the dedicated NOINHERIT maintenance pool.
    #[must_use]
    pub const fn new_maintenance(pool: PgPool) -> Self {
        Self {
            pool,
            assume_workspace_maintenance_role: true,
        }
    }

    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        if self.assume_workspace_maintenance_role {
            let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
            sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx_error)?;
            Ok(conn)
        } else {
            ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await
        }
    }

    /// Begins a tenant-scoped transaction for composing workspace and outbox writes.
    pub async fn begin_transaction(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        self.begin(tenant_id).await
    }
}

const WORKSPACE_COLUMNS: &str = "workspace_id, tenant_id, scope_kind, scope_session_id, \
    scope_worker_id, scope_run_id, scope_task_id, provider, provider_account_id, \
    provider_account_generation, durability_class, lifecycle_state, writer_epoch, \
    instance_generation, current_checkpoint_generation, current_checkpoint_id, \
    retention_deadline_at, delete_generation, access_fenced_at";

#[derive(Debug, Clone, Copy)]
struct WorkspaceBindingFence {
    provider_account_generation: i64,
    writer_epoch: i64,
    instance_generation: i64,
    checkpoint_generation: i64,
    checkpoint_id: Option<WorkspaceCheckpointId>,
}

impl TryFrom<&WorkspaceBinding> for WorkspaceBindingFence {
    type Error = MoaError;

    fn try_from(binding: &WorkspaceBinding) -> Result<Self> {
        let provider_account_generation = i64::try_from(binding.provider_account_generation)
            .map_err(|_| {
                MoaError::ValidationError(
                    "workspace provider-account generation overflows Postgres bigint".to_string(),
                )
            })?;
        let writer_epoch = i64::try_from(binding.writer_epoch).map_err(|_| {
            MoaError::ValidationError(
                "workspace writer epoch overflows Postgres bigint".to_string(),
            )
        })?;
        let instance_generation = i64::try_from(binding.instance_generation).map_err(|_| {
            MoaError::ValidationError(
                "workspace instance generation overflows Postgres bigint".to_string(),
            )
        })?;
        let (checkpoint_generation, checkpoint_id) = binding.current_revision.as_ref().map_or(
            Ok::<_, MoaError>((0_i64, None)),
            |revision| {
                let generation = i64::try_from(revision.generation).map_err(|_| {
                    MoaError::ValidationError(
                        "workspace checkpoint generation overflows Postgres bigint".to_string(),
                    )
                })?;
                if generation <= 0 || revision.format_version != CHECKPOINT_ARCHIVE_FORMAT_VERSION {
                    return Err(MoaError::ValidationError(
                        "workspace revision has an invalid generation or format".to_string(),
                    ));
                }
                Ok((generation, Some(revision.checkpoint_id)))
            },
        )?;
        if provider_account_generation <= 0 {
            return Err(MoaError::ValidationError(
                "workspace provider-account generation must be positive".to_string(),
            ));
        }
        Ok(Self {
            provider_account_generation,
            writer_epoch,
            instance_generation,
            checkpoint_generation,
            checkpoint_id,
        })
    }
}

fn validate_lease_for_binding(
    lease: &HandLease,
    binding: &WorkspaceBinding,
    expected_status: HandLeaseStatus,
) -> Result<()> {
    let fence = WorkspaceBindingFence::try_from(binding)?;
    let attachment_matches = lease.attachment.as_ref().is_some_and(|attachment| {
        attachment.workspace_id == binding.workspace_id
            && attachment.workspace_writer_epoch == fence.writer_epoch
            && attachment.workspace_instance_generation == fence.instance_generation
            && attachment.restored_checkpoint_id == fence.checkpoint_id
    });
    if lease.tenant_id != binding.tenant_id
        || lease.provider.trim().is_empty()
        || lease.status != expected_status
        || !attachment_matches
    {
        return Err(MoaError::ValidationError(
            "hand lease does not match the exact workspace binding and lifecycle state".to_string(),
        ));
    }
    Ok(())
}

type ScopeColumns = (
    &'static str,
    Option<SessionId>,
    Option<String>,
    Option<ExecutionRunScopeId>,
    Option<ExecutionTaskScopeId>,
);

fn scope_columns(scope: &SandboxWorkspaceScope) -> Result<ScopeColumns> {
    match scope {
        SandboxWorkspaceScope::Worker {
            session_id,
            worker_id,
        } if !worker_id.trim().is_empty() => Ok((
            "worker",
            Some(*session_id),
            Some(worker_id.clone()),
            None,
            None,
        )),
        SandboxWorkspaceScope::Worker { .. } => Err(MoaError::ValidationError(
            "worker workspace scope requires a nonempty worker id".to_string(),
        )),
        SandboxWorkspaceScope::ExecutionTask { run_id, task_id } => {
            Ok(("execution_task", None, None, Some(*run_id), Some(*task_id)))
        }
    }
}

fn workspace_from_row(row: &sqlx::postgres::PgRow) -> Result<SandboxWorkspace> {
    let scope_kind: String = row.try_get("scope_kind").map_err(map_sqlx_error)?;
    let scope = match scope_kind.as_str() {
        "worker" => SandboxWorkspaceScope::Worker {
            session_id: row.try_get("scope_session_id").map_err(map_sqlx_error)?,
            worker_id: row.try_get("scope_worker_id").map_err(map_sqlx_error)?,
        },
        "execution_task" => SandboxWorkspaceScope::ExecutionTask {
            run_id: row.try_get("scope_run_id").map_err(map_sqlx_error)?,
            task_id: row.try_get("scope_task_id").map_err(map_sqlx_error)?,
        },
        other => {
            return Err(MoaError::StorageError(format!(
                "unknown workspace scope kind: {other}"
            )));
        }
    };
    Ok(SandboxWorkspace {
        workspace_id: row.try_get("workspace_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        scope,
        provider: row.try_get("provider").map_err(map_sqlx_error)?,
        provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx_error)?,
        provider_account_generation: row
            .try_get("provider_account_generation")
            .map_err(map_sqlx_error)?,
        durability_class: DurabilityClass::from_label(
            &row.try_get::<String, _>("durability_class")
                .map_err(map_sqlx_error)?,
        )?,
        state: SandboxWorkspaceState::from_label(
            &row.try_get::<String, _>("lifecycle_state")
                .map_err(map_sqlx_error)?,
        )?,
        writer_epoch: row.try_get("writer_epoch").map_err(map_sqlx_error)?,
        instance_generation: row.try_get("instance_generation").map_err(map_sqlx_error)?,
        checkpoint_generation: row
            .try_get("current_checkpoint_generation")
            .map_err(map_sqlx_error)?,
        checkpoint_id: row
            .try_get("current_checkpoint_id")
            .map_err(map_sqlx_error)?,
        retention_deadline_at: row
            .try_get("retention_deadline_at")
            .map_err(map_sqlx_error)?,
        delete_generation: row.try_get("delete_generation").map_err(map_sqlx_error)?,
        access_fenced_at: row.try_get("access_fenced_at").map_err(map_sqlx_error)?,
    })
}
