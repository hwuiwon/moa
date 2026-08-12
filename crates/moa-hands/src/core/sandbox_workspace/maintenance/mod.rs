//! Cross-tenant sandbox-workspace retention, reconciliation, and purge owner.

mod inventory;
mod reconciliation;
mod retention;
mod tenant_purge;

use inventory::provider_metric_label;

#[cfg(test)]
use inventory::{
    DurableHandOwner, DurableInventory, DurableWorkspaceOwner, InventoryFindingKind,
    ProviderAccount, compare_inventory,
};
#[cfg(test)]
use reconciliation::reconciliation_resource_matches_claim;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_config::CheckpointRetentionConfig;
use moa_core::{
    error::{MoaError, Result},
    traits::{HandProvider, SandboxStorageProvider},
    types::{
        identifiers::{
            HandProvisioningOperationId, ProviderAccountId, SandboxWorkspaceId, TenantId,
            WorkspaceCheckpointId,
        },
        sandbox_workspace::{
            ProviderAccountStorageInventory, ProviderInventoryResourceKind, ProviderStorageKind,
            ProviderStorageRef, SandboxWorkspaceState, TenantStoragePurgeRequest,
            WorkspaceCapacityDimension, WorkspaceConfirmedDisposition, WorkspaceOperationOutcome,
            WorkspaceReconcileRequest, WorkspaceStorageOperation,
        },
    },
};
use moa_db::ScopedConn;
use moa_observability::{SandboxStorageResourceMetricState, SandboxWorkspaceInventoryDrift};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

use super::{
    checkpoint::store::{
        CheckpointEmptyObservation, CheckpointObjectStore, CheckpointPrefixObservation,
        CheckpointStoreContext,
    },
    failpoints,
    operations::{ClaimedWorkspaceOperation, PostgresWorkspaceOperationRepository},
    reaper::{WorkspaceInventoryObservation, WorkspaceReaper, WorkspaceReconciliationProbe},
};
use crate::core::{
    leases::{LeaseHandle, PostgresHandLeaseStore},
    telemetry::{
        record_workspace_active_hands, record_workspace_inventory_drift,
        record_workspace_parked_tasks_with_active_hands, record_workspace_quota_utilization,
        record_workspace_state, record_workspace_storage_resource_state,
    },
};

/// One bounded checkpoint-retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceRetentionPass {
    /// Checkpoints claimed by this replica.
    pub claimed: u64,
    /// Checkpoints durably tombstoned after verified object absence.
    pub deleted: u64,
    /// Checkpoints waiting for a separated second empty observation.
    pub awaiting_absence: u64,
    /// Checkpoints released behind retry backoff.
    pub retrying: u64,
}

/// One provider-inventory reconciliation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceInventoryPass {
    /// Provider-account generations exclusively claimed by this replica.
    pub accounts: u64,
    /// Provider resources observed.
    pub resources: u64,
    /// Unresolved findings after this complete pass.
    pub unresolved_findings: u64,
}

/// Current durable reconciliation backlog used by supervision and readiness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceMaintenanceBacklog {
    /// Operations currently eligible or waiting for reconciliation.
    pub count: u64,
    /// Age of the oldest outstanding operation.
    pub oldest_age: Duration,
}

/// Result of one external-first tenant purge pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceTenantPurgeProof {
    /// Exact tenant fenced before any external deletion.
    pub tenant_id: TenantId,
    /// Stable tenant-purge operation that owns this proof.
    pub operation_id: String,
    /// Number of exact hand resources proven absent.
    pub hands: u64,
    /// Number of exact provider storage resources proven absent.
    pub storage_resources: u64,
    /// Number of checkpoint prefixes proven absent.
    pub checkpoints: u64,
    /// Provider-account generations covered by separated inventory proofs.
    pub provider_accounts: u64,
    /// Digest of the exact tenant/account absence scope.
    pub provider_inventory_digest: String,
}

impl WorkspaceTenantPurgeProof {
    fn validate_for(&self, tenant_id: TenantId, operation_id: &str) -> Result<()> {
        if self.tenant_id != tenant_id
            || self.operation_id != operation_id
            || operation_id.trim().is_empty()
            || self.provider_inventory_digest.trim().is_empty()
        {
            return Err(MoaError::ValidationError(
                "workspace tenant purge proof does not match the exact tenant operation"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn evidence_digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"moa/sandbox-tenant-purge-proof/v1\0");
        digest.update(self.tenant_id.0.as_bytes());
        digest.update(self.operation_id.as_bytes());
        digest.update(self.hands.to_be_bytes());
        digest.update(self.storage_resources.to_be_bytes());
        digest.update(self.checkpoints.to_be_bytes());
        digest.update(self.provider_accounts.to_be_bytes());
        digest.update(self.provider_inventory_digest.as_bytes());
        format!("sha256:{:x}", digest.finalize())
    }
}

/// Process-wide owner for workspace retention, provider inventory, and purge.
#[derive(Clone)]
pub struct WorkspaceMaintenanceCoordinator {
    pool: PgPool,
    checkpoint_store: Arc<CheckpointObjectStore>,
    storage_providers: Arc<HashMap<String, Arc<dyn SandboxStorageProvider>>>,
    hand_providers: Arc<HashMap<String, Arc<dyn HandProvider>>>,
    retention: CheckpointRetentionConfig,
    reconciliation_claim_ttl: Duration,
    inventory_claim_owner: Uuid,
}

impl WorkspaceMaintenanceCoordinator {
    /// Verifies that a pool uses a dedicated non-inheriting maintenance login.
    ///
    /// The login must be an explicitly provisioned member of the named
    /// NOLOGIN role, but must not inherit its SECURITY DEFINER privileges until
    /// this coordinator deliberately uses `SET LOCAL ROLE`.
    pub async fn verify_maintenance_pool(pool: &PgPool) -> Result<()> {
        let mut transaction = pool.begin().await.map_err(map_sqlx)?;
        let (login, is_member, has_disallowed_attributes, inherits_execute): (
            String,
            bool,
            bool,
            bool,
        ) = sqlx::query_as(
            r#"
            WITH target_function AS (
                SELECT procedure.oid
                FROM pg_catalog.pg_proc AS procedure
                INNER JOIN pg_catalog.pg_namespace AS namespace
                    ON namespace.oid = procedure.pronamespace
                WHERE namespace.nspname = 'moa'
                  AND procedure.proname = 'fence_sandbox_workspaces_for_tenant_purge'
                  AND procedure.pronargs = 2
                  AND procedure.proargtypes[0] = 'uuid'::regtype
                  AND procedure.proargtypes[1] = 'text'::regtype
            )
            SELECT current_user::TEXT,
                   pg_has_role(current_user, 'moa_workspace_maintenance', 'MEMBER'),
                   NOT login_role.rolcanlogin
                       OR login_role.rolinherit
                       OR login_role.rolsuper
                       OR login_role.rolbypassrls
                       OR login_role.rolcreaterole
                       OR login_role.rolcreatedb
                       OR login_role.rolreplication,
                   COALESCE(
                       (
                           SELECT has_function_privilege(current_user, oid, 'EXECUTE')
                           FROM target_function
                       ),
                       FALSE
                   )
            FROM pg_catalog.pg_roles AS login_role
            WHERE login_role.rolname = current_user
            "#,
        )
        .fetch_one(&mut *transaction)
        .await
        .map_err(map_sqlx)?;
        if login == "moa_workspace_maintenance"
            || !is_member
            || has_disallowed_attributes
            || inherits_execute
        {
            return Err(MoaError::ConfigError(
                "workspace maintenance database login must be a distinct NOINHERIT member of moa_workspace_maintenance"
                    .to_string(),
            ));
        }
        sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
        let active_role: String = sqlx::query_scalar("SELECT current_user::TEXT")
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
        transaction.rollback().await.map_err(map_sqlx)?;
        if active_role != "moa_workspace_maintenance" {
            return Err(MoaError::ConfigError(
                "workspace maintenance database login cannot assume the exact maintenance role"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Builds one coordinator from the exact production provider registries.
    pub fn new(
        pool: PgPool,
        checkpoint_store: Arc<CheckpointObjectStore>,
        storage_providers: Vec<Arc<dyn SandboxStorageProvider>>,
        hand_providers: Vec<Arc<dyn HandProvider>>,
        retention: CheckpointRetentionConfig,
        reconciliation_claim_ttl: Duration,
    ) -> Result<Self> {
        let storage_providers = unique_storage_providers(storage_providers)?;
        let hand_providers = unique_hand_providers(hand_providers)?;
        if storage_providers.is_empty()
            || hand_providers.is_empty()
            || retention.gc_batch_size == 0
            || retention.claim_ttl_seconds == 0
            || retention.retry_backoff_seconds == 0
            || reconciliation_claim_ttl.is_zero()
        {
            return Err(MoaError::ConfigError(
                "workspace maintenance requires providers and positive retention bounds"
                    .to_string(),
            ));
        }
        Ok(Self {
            pool,
            checkpoint_store,
            storage_providers: Arc::new(storage_providers),
            hand_providers: Arc::new(hand_providers),
            retention,
            reconciliation_claim_ttl,
            inventory_claim_owner: Uuid::now_v7(),
        })
    }

    /// Returns the durable reconciliation backlog and oldest age.
    pub async fn backlog(&self) -> Result<WorkspaceMaintenanceBacklog> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let row = sqlx::query(
            r#"
            SELECT count(*)::BIGINT AS count,
                   COALESCE(EXTRACT(EPOCH FROM now() - min(created_at)), 0)::DOUBLE PRECISION AS oldest
            FROM moa.sandbox_workspace_operations
            WHERE outcome_class = 'unknown'
               OR (outcome_class = 'not_sent' AND deadline_at <= now())
            "#,
        )
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        let count: i64 = row.try_get("count").map_err(map_sqlx)?;
        let oldest: f64 = row.try_get("oldest").map_err(map_sqlx)?;
        conn.commit().await?;
        Ok(WorkspaceMaintenanceBacklog {
            count: u64::try_from(count.max(0)).unwrap_or(0),
            oldest_age: Duration::from_secs_f64(oldest.max(0.0)),
        })
    }

    /// Emits a complete zero-filled workspace fleet metric snapshot.
    pub async fn emit_fleet_metrics(&self) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT provider, lifecycle_state, count(*)::BIGINT AS count FROM moa.sandbox_workspaces GROUP BY provider, lifecycle_state",
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let mut counts = HashMap::new();
        for row in rows {
            *counts
                .entry((
                    provider_metric_label(&row.try_get::<String, _>("provider").map_err(map_sqlx)?)
                        .to_string(),
                    row.try_get::<String, _>("lifecycle_state")
                        .map_err(map_sqlx)?,
                ))
                .or_insert(0) +=
                u64::try_from(row.try_get::<i64, _>("count").map_err(map_sqlx)?).unwrap_or(0);
        }
        for provider in ["local", "daytona", "e2b", "other"] {
            for state in all_workspace_states() {
                let count = counts
                    .get(&(provider.to_string(), state.as_str().to_string()))
                    .copied()
                    .unwrap_or(0);
                record_workspace_state(provider, state, count);
            }
        }
        self.emit_storage_resource_metrics().await?;
        self.emit_active_hand_metrics().await?;
        self.emit_quota_utilization_metrics().await?;
        self.emit_parked_task_compute_violations().await?;
        Ok(())
    }

    /// Emits a zero-filled durable storage-resource fleet snapshot.
    async fn emit_storage_resource_metrics(&self) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT provider_account.provider AS provider, resource.lifecycle_state, \
                    count(*)::BIGINT AS count \
             FROM moa.sandbox_storage_resources AS resource \
             JOIN moa.sandbox_provider_accounts AS provider_account \
               ON provider_account.provider_account_id = resource.provider_account_id \
             GROUP BY provider_account.provider, resource.lifecycle_state",
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let mut counts = HashMap::new();
        for row in rows {
            let provider =
                provider_metric_label(&row.try_get::<String, _>("provider").map_err(map_sqlx)?)
                    .to_string();
            let state = row
                .try_get::<String, _>("lifecycle_state")
                .map_err(map_sqlx)?;
            *counts.entry((provider, state)).or_insert(0u64) +=
                u64::try_from(row.try_get::<i64, _>("count").map_err(map_sqlx)?).unwrap_or(0);
        }
        // Zero-fill every provider/state pair: the alerts on these series are
        // `absent()`-guarded, so a gauge written only when rows exist would page on a
        // healthy fleet that simply has no storage resources in that state.
        for provider in ["local", "daytona", "e2b", "other"] {
            for state in all_storage_resource_states() {
                let count = counts
                    .get(&(provider.to_string(), state.as_str().to_string()))
                    .copied()
                    .unwrap_or(0);
                record_workspace_storage_resource_state(provider, state, count);
            }
        }
        Ok(())
    }

    /// Emits the per-provider count of sandbox compute instances holding capacity.
    async fn emit_active_hand_metrics(&self) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT provider_account.provider AS provider, count(*)::BIGINT AS count \
             FROM moa.sandbox_capacity_reservations AS reservation \
             JOIN moa.sandbox_provider_accounts AS provider_account \
               ON provider_account.provider_account_id = reservation.provider_account_id \
             WHERE reservation.resource_dimension = 'active_hands' \
               AND reservation.reservation_state <> 'released' \
             GROUP BY provider_account.provider",
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let mut counts = HashMap::new();
        for row in rows {
            let provider =
                provider_metric_label(&row.try_get::<String, _>("provider").map_err(map_sqlx)?)
                    .to_string();
            *counts.entry(provider).or_insert(0u64) +=
                u64::try_from(row.try_get::<i64, _>("count").map_err(map_sqlx)?).unwrap_or(0);
        }
        for provider in ["local", "daytona", "e2b", "other"] {
            record_workspace_active_hands(provider, counts.get(provider).copied().unwrap_or(0));
        }
        Ok(())
    }

    /// Emits the fleet-wide utilization ratio for every capacity dimension.
    async fn emit_quota_utilization_metrics(&self) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT reservation.resource_dimension, \
                    sum(reservation.quantity)::BIGINT AS reserved, \
                    max(limits.limit_value)::BIGINT AS limit_value \
             FROM moa.sandbox_capacity_reservations AS reservation \
             LEFT JOIN moa.sandbox_tenant_capacity_limits AS limits \
               ON limits.tenant_id = reservation.tenant_id \
              AND limits.resource_dimension = reservation.resource_dimension \
             WHERE reservation.reservation_state <> 'released' \
             GROUP BY reservation.resource_dimension",
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        let mut ratios = HashMap::new();
        for row in rows {
            let dimension = row
                .try_get::<String, _>("resource_dimension")
                .map_err(map_sqlx)?;
            let reserved = row.try_get::<i64, _>("reserved").map_err(map_sqlx)?.max(0);
            let limit = row
                .try_get::<Option<i64>, _>("limit_value")
                .map_err(map_sqlx)?
                .unwrap_or(0);
            // An absent or zero limit means the dimension is unbounded for every tenant
            // observed, which is 0.0 pressure rather than a division by zero.
            let ratio = if limit > 0 {
                reserved as f64 / limit as f64
            } else {
                0.0
            };
            ratios.insert(dimension, ratio);
        }
        for dimension in all_capacity_dimensions() {
            let ratio = ratios.get(dimension.as_str()).copied().unwrap_or(0.0);
            record_workspace_quota_utilization(dimension, ratio);
        }
        Ok(())
    }

    /// Emits the count of parked execution tasks that still own sandbox compute.
    ///
    /// This is the only automated guard on the invariant that a parked run owns no
    /// sandbox. A `pending` release receipt is excluded deliberately: that row marks the
    /// legitimate in-flight checkpoint-and-release window, so counting it would make every
    /// normal yield trip a critical alert.
    async fn emit_parked_task_compute_violations(&self) -> Result<()> {
        let mut conn = maintenance_conn(&self.pool).await?;
        let violations: i64 = sqlx::query_scalar(
            "SELECT count(*)::BIGINT \
             FROM moa.hand_leases AS lease \
             JOIN moa.sandbox_workspaces AS workspace \
               ON workspace.workspace_id = lease.workspace_id \
              AND workspace.tenant_id = lease.tenant_id \
             JOIN moa.execution_task AS task \
               ON task.task_id = workspace.scope_task_id \
              AND task.run_uid = workspace.scope_run_id \
              AND task.tenant_id = workspace.tenant_id \
             WHERE lease.status IN ('provisioning', 'active') \
               AND workspace.scope_kind = 'execution_task' \
               AND task.status IN ( \
                     'waiting_input', 'waiting_review', 'waiting_signal', \
                     'waiting_timer', 'waiting_external', 'waiting_replan') \
               AND NOT EXISTS ( \
                     SELECT 1 FROM moa.sandbox_execution_hand_release_receipts AS receipt \
                     WHERE receipt.tenant_id = task.tenant_id \
                       AND receipt.run_uid = task.run_uid \
                       AND receipt.task_id = task.task_id \
                       AND receipt.owner_kind = 'task' \
                       AND receipt.receipt_state = 'pending')",
        )
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await?;
        record_workspace_parked_tasks_with_active_hands(
            u64::try_from(violations.max(0)).unwrap_or(0),
        );
        Ok(())
    }

    fn storage_provider(&self, name: &str) -> Result<Arc<dyn SandboxStorageProvider>> {
        self.storage_providers.get(name).cloned().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "workspace maintenance has no storage provider for `{name}`"
            ))
        })
    }

    fn hand_provider(&self, name: &str) -> Result<Arc<dyn HandProvider>> {
        self.hand_providers.get(name).cloned().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "workspace maintenance has no hand provider for `{name}`"
            ))
        })
    }
}

fn unique_storage_providers(
    providers: Vec<Arc<dyn SandboxStorageProvider>>,
) -> Result<HashMap<String, Arc<dyn SandboxStorageProvider>>> {
    let mut result = HashMap::new();
    for provider in providers {
        let name = provider.storage_provider_name().trim();
        if name.is_empty() || result.insert(name.to_string(), provider).is_some() {
            return Err(MoaError::ConfigError(
                "workspace maintenance storage providers must have unique names".to_string(),
            ));
        }
    }
    Ok(result)
}

fn unique_hand_providers(
    providers: Vec<Arc<dyn HandProvider>>,
) -> Result<HashMap<String, Arc<dyn HandProvider>>> {
    let mut result = HashMap::new();
    for provider in providers {
        let name = provider.provider_name().trim();
        if name.is_empty() || result.insert(name.to_string(), provider).is_some() {
            return Err(MoaError::ConfigError(
                "workspace maintenance hand providers must have unique names".to_string(),
            ));
        }
    }
    Ok(result)
}

async fn maintenance_conn(pool: &PgPool) -> Result<ScopedConn<'_>> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query("SET LOCAL ROLE moa_workspace_maintenance")
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
    Ok(conn)
}

fn all_workspace_states() -> [SandboxWorkspaceState; 10] {
    [
        SandboxWorkspaceState::Creating,
        SandboxWorkspaceState::Ready,
        SandboxWorkspaceState::Active,
        SandboxWorkspaceState::Quiescing,
        SandboxWorkspaceState::Committing,
        SandboxWorkspaceState::Restoring,
        SandboxWorkspaceState::Reconciling,
        SandboxWorkspaceState::Failed,
        SandboxWorkspaceState::Deleting,
        SandboxWorkspaceState::Deleted,
    ]
}

fn all_storage_resource_states() -> [SandboxStorageResourceMetricState; 7] {
    [
        SandboxStorageResourceMetricState::Creating,
        SandboxStorageResourceMetricState::Ready,
        SandboxStorageResourceMetricState::Attached,
        SandboxStorageResourceMetricState::Deleting,
        SandboxStorageResourceMetricState::Deleted,
        SandboxStorageResourceMetricState::Unknown,
        SandboxStorageResourceMetricState::Failed,
    ]
}

fn all_capacity_dimensions() -> [WorkspaceCapacityDimension; 5] {
    [
        WorkspaceCapacityDimension::Workspaces,
        WorkspaceCapacityDimension::ActiveHands,
        WorkspaceCapacityDimension::Volumes,
        WorkspaceCapacityDimension::Checkpoints,
        WorkspaceCapacityDimension::LogicalBytes,
    ]
}

fn map_sqlx(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_purge_confirmation_rejects_cross_tenant_or_operation_proof_offline() {
        // Pins: phase two cannot turn a journaled proof for another destruction
        // fence into the SECURITY DEFINER database confirmation authority.
        let tenant_id = TenantId::new();
        let operation_id = "tenant-purge-operation";
        let proof = WorkspaceTenantPurgeProof {
            tenant_id,
            operation_id: operation_id.to_string(),
            hands: 0,
            storage_resources: 0,
            checkpoints: 0,
            provider_accounts: 0,
            provider_inventory_digest: "sha256:empty-inventory".to_string(),
        };
        proof
            .validate_for(tenant_id, operation_id)
            .expect("the exact journaled proof validates");
        assert!(proof.validate_for(TenantId::new(), operation_id).is_err());
        assert!(
            proof
                .validate_for(tenant_id, "competing-operation")
                .is_err()
        );
        let mut missing_digest = proof;
        missing_digest.provider_inventory_digest.clear();
        assert!(
            missing_digest
                .validate_for(tenant_id, operation_id)
                .is_err()
        );
    }

    #[test]
    fn tenant_purge_proof_round_trips_through_restate_json_offline() {
        // Pins: the phase-one result is a complete secret-free Restate run
        // value that can be journaled and reconstructed before phase two.
        let proof = WorkspaceTenantPurgeProof {
            tenant_id: TenantId::new(),
            operation_id: "tenant-purge-journal".to_string(),
            hands: 3,
            storage_resources: 2,
            checkpoints: 5,
            provider_accounts: 1,
            provider_inventory_digest: "sha256:provider-empty".to_string(),
        };
        let encoded = serde_json::to_vec(&proof).expect("serialize journaled purge proof");
        assert!(!String::from_utf8_lossy(&encoded).contains("credential"));
        let decoded: WorkspaceTenantPurgeProof =
            serde_json::from_slice(&encoded).expect("deserialize journaled purge proof");
        assert_eq!(decoded, proof);
    }

    fn claimed_operation(
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
    ) -> ClaimedWorkspaceOperation {
        let now = Utc::now();
        ClaimedWorkspaceOperation {
            operation: crate::core::sandbox_workspace::operations::WorkspaceOperation {
                operation_id: moa_core::types::identifiers::WorkspaceOperationId::new(),
                tenant_id,
                workspace_id,
                provider_account_id: ProviderAccountId::new(),
                provider_account_generation: 1,
                kind: moa_core::types::sandbox_workspace::WorkspaceOperationKind::Create,
                request_hash: "sha256:reconciliation-owner-test".to_string(),
                expected_writer_epoch: 7,
                expected_instance_generation: 11,
                expected_checkpoint_generation: 0,
                deadline_at: now,
                reconcile_not_before: now,
                outcome: moa_core::types::sandbox_workspace::WorkspaceOperationOutcome::Unknown,
                confirmed_disposition: None,
                absence_observation_count: 0,
                absence_first_observed_at: None,
                absence_last_observed_at: None,
                absence_inventory_digest: None,
                claim_token: Some(Uuid::now_v7()),
                claim_expires_at: Some(now),
                attempts: 0,
            },
            claim_token: Uuid::now_v7(),
        }
    }

    #[test]
    fn inventory_comparison_quarantines_wrong_account_duplicate_and_missing_offline() {
        // Pins: provider inventory never auto-deletes an unrecognized resource,
        // and a missing durable resource remains a separate actionable finding.
        let account = ProviderAccount {
            id: ProviderAccountId::new(),
            generation: 1,
            provider: "e2b".to_string(),
        };
        let tenant_id = TenantId::new();
        let workspace_id = SandboxWorkspaceId::new();
        let owner = moa_core::types::sandbox_workspace::ProviderInventoryOwner {
            tenant_id,
            workspace_id,
            provisioning_operation_id: Some(HandProvisioningOperationId::new()),
            writer_epoch: Some(1),
            instance_generation: Some(1),
        };
        let inventory = ProviderAccountStorageInventory {
            provider_account_id: account.id,
            provider_account_generation: account.generation,
            observed_at: Utc::now(),
            resources: ["one", "two"]
                .into_iter()
                .map(
                    |reference| moa_core::types::sandbox_workspace::ProviderInventoryResource {
                        kind: ProviderInventoryResourceKind::Compute,
                        provider_reference: reference.to_string(),
                        resource_fingerprint: format!("fingerprint-{reference}"),
                        evidence_digest: format!("evidence-{reference}"),
                        verified_owner: Some(owner.clone()),
                    },
                )
                .collect(),
        };
        let durable = DurableInventory {
            storage: HashMap::from([("missing-volume".to_string(), tenant_id)]),
            workspaces: HashMap::from([(
                workspace_id,
                DurableWorkspaceOwner {
                    tenant_id,
                    provider_account_id: ProviderAccountId::new(),
                    provider_account_generation: 1,
                    writer_epoch: 1,
                    instance_generation: 1,
                },
            )]),
            hands: HashMap::from([(
                owner
                    .provisioning_operation_id
                    .expect("test compute owner has provisioning operation"),
                DurableHandOwner {
                    tenant_id,
                    workspace_id,
                },
            )]),
        };
        let findings = compare_inventory(&account, &inventory, &durable);
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.kind == InventoryFindingKind::WrongAccount)
                .count(),
            2
        );
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.kind == InventoryFindingKind::Duplicate)
                .count(),
            2
        );
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.kind == InventoryFindingKind::Missing)
                .count(),
            1
        );
    }

    #[test]
    fn reconciliation_reference_cannot_override_owner_generation_or_operation_offline() {
        // Pins: a recycled exact provider reference is not proof for a newer
        // tenant/workspace generation, and compute additionally requires the
        // durable hand provisioning operation carried by provider metadata.
        let tenant_id = TenantId::new();
        let workspace_id = SandboxWorkspaceId::new();
        let claimed = claimed_operation(tenant_id, workspace_id);
        let provisioning_operation_id = HandProvisioningOperationId::new();
        let mut resource = moa_core::types::sandbox_workspace::ProviderInventoryResource {
            kind: ProviderInventoryResourceKind::Compute,
            provider_reference: "recycled-reference".to_string(),
            resource_fingerprint: "sha256:resource".to_string(),
            evidence_digest: "sha256:evidence".to_string(),
            verified_owner: Some(moa_core::types::sandbox_workspace::ProviderInventoryOwner {
                tenant_id: TenantId::new(),
                workspace_id,
                provisioning_operation_id: Some(provisioning_operation_id),
                writer_epoch: Some(7),
                instance_generation: Some(11),
            }),
        };
        assert!(!reconciliation_resource_matches_claim(
            &resource,
            &claimed,
            Some("recycled-reference"),
            &[provisioning_operation_id.0],
        ));

        resource
            .verified_owner
            .as_mut()
            .expect("test resource owner")
            .tenant_id = tenant_id;
        assert!(reconciliation_resource_matches_claim(
            &resource,
            &claimed,
            Some("recycled-reference"),
            &[provisioning_operation_id.0],
        ));
        assert!(!reconciliation_resource_matches_claim(
            &resource,
            &claimed,
            Some("recycled-reference"),
            &[],
        ));
    }
}
