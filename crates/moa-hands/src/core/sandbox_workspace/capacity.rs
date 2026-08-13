//! Atomic tenant and provider-account workspace-capacity admission.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{
            HandProvisioningOperationId, ProviderAccountId, SandboxWorkspaceId, TenantId,
            WorkspaceOperationId,
        },
        sandbox_workspace::{
            WorkspaceCapacityDimension, WorkspaceOperationKind, WorkspaceStorageOperation,
        },
    },
};
use moa_db::ScopedConn;
use serde_json::Value;
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

use moa_observability::SandboxWorkspaceQuotaDecision;

use crate::core::{leases::map_sqlx_error, telemetry::record_workspace_quota_decision};

/// One positive capacity quantity requested by an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityQuantity {
    /// Capacity dimension.
    pub dimension: WorkspaceCapacityDimension,
    /// Positive quantity to reserve.
    pub quantity: u64,
}

/// Complete fenced atomic capacity request.
#[derive(Debug, Clone)]
pub struct CapacityReservationRequest {
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Workspace consuming capacity.
    pub workspace_id: SandboxWorkspaceId,
    /// Intent whose provider request will consume the capacity.
    pub operation_id: WorkspaceOperationId,
    /// Provider account and isolation cell.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account generation.
    pub provider_account_generation: i64,
    /// Exact writer fence carried by the operation.
    pub expected_writer_epoch: i64,
    /// Exact compute-instance fence carried by the operation.
    pub expected_instance_generation: i64,
    /// All dimensions reserved atomically.
    pub quantities: Vec<CapacityQuantity>,
}

/// One durable capacity reservation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityReservation {
    /// Reservation row identity.
    pub reservation_id: Uuid,
    /// Reserved dimension.
    pub dimension: WorkspaceCapacityDimension,
    /// Reserved positive quantity.
    pub quantity: u64,
}

/// Exact active-compute reservation owned by one hand lease generation.
#[derive(Debug, Clone, Copy)]
pub struct ActiveHandCapacityRequest {
    /// Verified tenant owner.
    pub tenant_id: TenantId,
    /// Workspace whose writer owns the compute.
    pub workspace_id: SandboxWorkspaceId,
    /// Provider account and isolation cell.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account generation.
    pub provider_account_generation: i64,
    /// Provider-visible idempotent hand creation identity.
    pub provisioning_operation_id: HandProvisioningOperationId,
    /// Exact durable hand lease generation.
    pub hand_lease_generation: i64,
    /// Exact workspace writer fence.
    pub expected_writer_epoch: i64,
    /// Exact workspace compute-instance fence.
    pub expected_instance_generation: i64,
}

/// Postgres-backed atomic capacity admission repository.
#[derive(Clone)]
pub struct PostgresWorkspaceCapacityRepository {
    pool: PgPool,
}

impl PostgresWorkspaceCapacityRepository {
    /// Creates a capacity repository over the runtime Postgres pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Begins the closed trusted admission transaction.
    ///
    /// This owner must atomically sum reservations across tenants for one
    /// provider account, so it uses the same closed control-plane mechanism as
    /// fleet reapers. Every tenant-owned predicate and inserted row still
    /// repeats the verified tenant and exact operation fence.
    async fn begin(&self) -> Result<ScopedConn<'_>> {
        let mut conn = ScopedConn::begin_control_plane(&self.pool).await?;
        conn.assume_app_role().await?;
        Ok(conn)
    }

    /// Reserves every requested dimension or none of them.
    ///
    /// Transactions always lock tenant scope before provider-account scope.
    /// Database advisory locks make this order cross-replica; no process mutex
    /// participates in correctness.
    pub async fn reserve(
        &self,
        request: &CapacityReservationRequest,
    ) -> Result<Vec<CapacityReservation>> {
        self.reserve_with_expiry(request, None).await
    }

    async fn reserve_with_expiry(
        &self,
        request: &CapacityReservationRequest,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Vec<CapacityReservation>> {
        let quantities = validated_quantities(request)?;
        let mut conn = self.begin().await?;

        // Fixed global order: tenant, then provider account/isolation cell.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("sandbox-capacity:tenant:{}", request.tenant_id))
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "sandbox-capacity:provider:{}",
                request.provider_account_id
            ))
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;

        let operation_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM moa.sandbox_workspace_operations AS operation
                JOIN moa.sandbox_workspaces AS workspace
                  ON workspace.tenant_id = operation.tenant_id
                 AND workspace.workspace_id = operation.workspace_id
                WHERE operation.tenant_id = $1 AND operation.workspace_id = $2
                  AND operation.operation_id = $3
                  AND operation.provider_account_id = $4
                  AND operation.provider_account_generation = $5
                  AND operation.expected_writer_epoch = $6
                  AND operation.expected_instance_generation = $7
                  AND (
                      operation.outcome_class = 'not_sent'
                      OR (
                          operation.outcome_class = 'unknown'
                          AND operation.operation_kind IN ('commit', 'checkpoint')
                          AND workspace.lifecycle_state = 'committing'
                      )
                  )
            )
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.workspace_id)
        .bind(request.operation_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if !operation_exists {
            return Err(MoaError::StorageError(
                "capacity reservation lost its exact operation or generation fence".to_string(),
            ));
        }

        let existing = load_operation_reservations(conn.as_mut(), request).await?;
        if !existing.is_empty() {
            if existing.len() != quantities.len()
                || existing.iter().any(|reservation| {
                    quantities.get(&reservation.dimension).copied()
                        != i64::try_from(reservation.quantity).ok()
                })
            {
                return Err(MoaError::StorageError(
                    "capacity reservation replay changed its exact dimensions or quantities"
                        .to_string(),
                ));
            }
            conn.commit().await?;
            return Ok(existing);
        }

        enforce_capacity(
            conn.as_mut(),
            request.tenant_id,
            request.provider_account_id,
            request.provider_account_generation,
            &quantities,
        )
        .await?;

        let mut reservations = Vec::with_capacity(quantities.len());
        for (dimension, quantity) in quantities {
            let reservation_id = Uuid::now_v7();
            let row = sqlx::query(
                r#"
                INSERT INTO moa.sandbox_capacity_reservations (
                    reservation_id, tenant_id, provider_account_id,
                    provider_account_generation, workspace_id, operation_id,
                    expected_writer_epoch, expected_instance_generation,
                    resource_dimension, quantity, expires_at
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                ON CONFLICT (tenant_id, operation_id, resource_dimension)
                    WHERE operation_id IS NOT NULL
                DO UPDATE SET updated_at = moa.sandbox_capacity_reservations.updated_at
                WHERE moa.sandbox_capacity_reservations.workspace_id = EXCLUDED.workspace_id
                  AND moa.sandbox_capacity_reservations.provider_account_id = EXCLUDED.provider_account_id
                  AND moa.sandbox_capacity_reservations.provider_account_generation = EXCLUDED.provider_account_generation
                  AND moa.sandbox_capacity_reservations.expected_writer_epoch = EXCLUDED.expected_writer_epoch
                  AND moa.sandbox_capacity_reservations.expected_instance_generation = EXCLUDED.expected_instance_generation
                  AND moa.sandbox_capacity_reservations.quantity = EXCLUDED.quantity
                  AND moa.sandbox_capacity_reservations.reservation_state IN ('pending', 'committed', 'reconciling')
                RETURNING reservation_id
                "#,
            )
            .bind(reservation_id)
            .bind(request.tenant_id)
            .bind(request.provider_account_id)
            .bind(request.provider_account_generation)
            .bind(request.workspace_id)
            .bind(request.operation_id)
            .bind(request.expected_writer_epoch)
            .bind(request.expected_instance_generation)
            .bind(dimension.as_str())
            .bind(quantity)
            .bind(expires_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?
            .ok_or_else(|| {
                MoaError::StorageError(
                    "conflicting replay attempted to reuse an operation capacity identity"
                        .to_string(),
                )
            })?;
            reservations.push(CapacityReservation {
                reservation_id: row.try_get("reservation_id").map_err(map_sqlx_error)?,
                dimension,
                quantity: u64::try_from(quantity).map_err(|_| {
                    MoaError::StorageError("negative persisted capacity quantity".to_string())
                })?,
            });
        }
        conn.commit().await?;
        Ok(reservations)
    }

    /// Reserves provider-independent checkpoint count and logical bytes before upload.
    ///
    /// The caller supplies the logical byte count from the already-built,
    /// bounded portable archive. A zero-byte archive reserves only its one
    /// immutable checkpoint row because reservation quantities are positive.
    pub async fn reserve_checkpoint_publication(
        &self,
        operation: &WorkspaceStorageOperation,
        logical_bytes: u64,
    ) -> Result<Vec<CapacityReservation>> {
        if !matches!(
            operation.kind,
            WorkspaceOperationKind::Commit | WorkspaceOperationKind::Checkpoint
        ) {
            return Err(MoaError::ValidationError(
                "checkpoint capacity requires a commit or checkpoint operation".to_string(),
            ));
        }
        let mut request = CapacityReservationRequest {
            tenant_id: operation.binding.tenant_id,
            workspace_id: operation.binding.workspace_id,
            operation_id: operation.operation_id,
            provider_account_id: operation.binding.provider_account_id,
            provider_account_generation: i64::try_from(
                operation.binding.provider_account_generation,
            )
            .map_err(|_| {
                MoaError::ValidationError(
                    "checkpoint provider-account generation overflows Postgres bigint".to_string(),
                )
            })?,
            expected_writer_epoch: i64::try_from(operation.binding.writer_epoch).map_err(|_| {
                MoaError::ValidationError(
                    "checkpoint writer epoch overflows Postgres bigint".to_string(),
                )
            })?,
            expected_instance_generation: i64::try_from(operation.binding.instance_generation)
                .map_err(|_| {
                    MoaError::ValidationError(
                        "checkpoint instance generation overflows Postgres bigint".to_string(),
                    )
                })?,
            quantities: Vec::with_capacity(2),
        };
        request.quantities.push(CapacityQuantity {
            dimension: WorkspaceCapacityDimension::Checkpoints,
            quantity: 1,
        });
        if logical_bytes > 0 {
            request.quantities.push(CapacityQuantity {
                dimension: WorkspaceCapacityDimension::LogicalBytes,
                quantity: logical_bytes,
            });
        }
        self.reserve_with_expiry(&request, Some(operation.deadline))
            .await
    }

    /// Reserves one active hand before its exact provider creation operation starts.
    pub async fn reserve_active_hand(
        &self,
        request: &ActiveHandCapacityRequest,
    ) -> Result<CapacityReservation> {
        validate_active_hand_request(request)?;
        let mut conn = self.begin().await?;
        lock_capacity_scope_values(
            conn.as_mut(),
            request.tenant_id,
            request.provider_account_id,
        )
        .await?;
        let lease_exists = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT TRUE
            FROM moa.hand_leases AS lease
            JOIN moa.sandbox_workspaces AS workspace
              ON workspace.tenant_id = lease.tenant_id
             AND workspace.workspace_id = lease.workspace_id
            WHERE lease.tenant_id = $1
              AND lease.provisioning_operation_id = $2
              AND lease.generation = $3
              AND lease.status = 'provisioning'
              AND lease.handle IS NULL
              AND lease.workspace_id = $4
              AND lease.workspace_writer_epoch = $5
              AND lease.workspace_instance_generation = $6
              AND workspace.provider = lease.provider
              AND workspace.provider_account_id = $7
              AND workspace.provider_account_generation = $8
              AND workspace.writer_epoch = $5
              AND workspace.instance_generation = $6
              AND workspace.lifecycle_state = 'restoring'
              AND workspace.access_fenced_at IS NULL
            FOR UPDATE OF lease, workspace
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.provisioning_operation_id)
        .bind(request.hand_lease_generation)
        .bind(request.workspace_id)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .unwrap_or(false);
        if !lease_exists {
            return Err(MoaError::StorageError(
                "active-hand reservation lost its exact lease or workspace generation fence"
                    .to_string(),
            ));
        }

        let existing = load_active_hand_reservation(conn.as_mut(), request).await?;
        if let Some(existing) = existing {
            conn.commit().await?;
            return Ok(existing);
        }

        let quantities = BTreeMap::from([(WorkspaceCapacityDimension::ActiveHands, 1_i64)]);
        enforce_capacity(
            conn.as_mut(),
            request.tenant_id,
            request.provider_account_id,
            request.provider_account_generation,
            &quantities,
        )
        .await?;
        let reservation_id = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO moa.sandbox_capacity_reservations (
                reservation_id, tenant_id, provider_account_id,
                provider_account_generation, workspace_id, operation_id,
                expected_writer_epoch, expected_instance_generation,
                resource_dimension, quantity, hand_provisioning_operation_id,
                hand_lease_generation
            ) VALUES ($1, $2, $3, $4, $5, NULL, $6, $7,
                      'active_hands', 1, $8, $9)
            ON CONFLICT (tenant_id, hand_provisioning_operation_id, resource_dimension)
                WHERE resource_dimension = 'active_hands'
            DO UPDATE SET updated_at = moa.sandbox_capacity_reservations.updated_at
            WHERE moa.sandbox_capacity_reservations.workspace_id = EXCLUDED.workspace_id
              AND moa.sandbox_capacity_reservations.provider_account_id = EXCLUDED.provider_account_id
              AND moa.sandbox_capacity_reservations.provider_account_generation = EXCLUDED.provider_account_generation
              AND moa.sandbox_capacity_reservations.expected_writer_epoch = EXCLUDED.expected_writer_epoch
              AND moa.sandbox_capacity_reservations.expected_instance_generation = EXCLUDED.expected_instance_generation
              AND moa.sandbox_capacity_reservations.hand_lease_generation = EXCLUDED.hand_lease_generation
              AND moa.sandbox_capacity_reservations.reservation_state IN ('pending', 'committed', 'reconciling')
            RETURNING reservation_id, reservation_state
            "#,
        )
        .bind(reservation_id)
        .bind(request.tenant_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.workspace_id)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .bind(request.provisioning_operation_id)
        .bind(request.hand_lease_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError(
                "conflicting replay attempted to reuse an active-hand capacity identity"
                    .to_string(),
            )
        })?;
        let reservation_id = row.try_get("reservation_id").map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(CapacityReservation {
            reservation_id,
            dimension: WorkspaceCapacityDimension::ActiveHands,
            quantity: 1,
        })
    }

    /// Commits active-hand capacity only after the exact lease is active.
    pub async fn commit_active_hand(&self, request: &ActiveHandCapacityRequest) -> Result<bool> {
        validate_active_hand_request(request)?;
        let mut conn = self.begin().await?;
        let changed = commit_active_hand_in_transaction(conn.as_mut(), request).await?;
        conn.commit().await?;
        Ok(changed)
    }

    /// Commits active-hand capacity, treating an earlier identical commit as done.
    ///
    /// Activation replays legitimately find the reservation already `committed`,
    /// which [`Self::commit_active_hand`] reports as "no row changed". Only a
    /// lost writer-epoch, instance-generation, or lease fence leaves the charge
    /// uncommitted, and the caller must not proceed on that.
    pub async fn ensure_active_hand_committed(
        &self,
        request: &ActiveHandCapacityRequest,
    ) -> Result<bool> {
        validate_active_hand_request(request)?;
        let mut conn = self.begin().await?;
        let committed = if commit_active_hand_in_transaction(conn.as_mut(), request).await? {
            true
        } else {
            active_hand_reservation_state(conn.as_mut(), request)
                .await?
                .as_deref()
                == Some("committed")
        };
        conn.commit().await?;
        Ok(committed)
    }

    /// Releases active-hand capacity after exact durable reaper ownership is established.
    pub async fn release_active_hand_to_reaper(
        &self,
        request: &ActiveHandCapacityRequest,
        claim_token: Uuid,
    ) -> Result<bool> {
        validate_active_hand_request(request)?;
        let mut conn = self.begin().await?;
        let owns_reaping = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT TRUE
            FROM moa.hand_leases
            WHERE tenant_id = $1
              AND provisioning_operation_id = $2
              AND generation = $3
              AND status = 'reaping'
              AND reap_claim_token = $4
              AND reap_claim_expires_at > now()
            FOR UPDATE
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.provisioning_operation_id)
        .bind(request.hand_lease_generation)
        .bind(claim_token)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .unwrap_or(false);
        if !owns_reaping {
            conn.rollback().await?;
            return Ok(false);
        }
        let changed = release_active_hand_row(conn.as_mut(), request).await?;
        conn.commit().await?;
        Ok(changed)
    }

    /// Releases one logical workspace charge after exact deletion finalization.
    pub async fn release_workspace(
        &self,
        tenant_id: TenantId,
        workspace_id: SandboxWorkspaceId,
        delete_generation: i64,
    ) -> Result<bool> {
        if delete_generation <= 0 {
            return Err(MoaError::ValidationError(
                "workspace capacity release requires a positive delete generation".to_string(),
            ));
        }
        let mut conn = self.begin().await?;
        let changed = release_workspace_in_transaction(
            conn.as_mut(),
            tenant_id,
            workspace_id,
            delete_generation,
        )
        .await?;
        conn.commit().await?;
        Ok(changed)
    }

    /// Atomically reserves one lifetime Daytona volume without double-counting inventory.
    ///
    /// Live durable rows, provider-observed IDs, and pending/unknown operation
    /// reservations are reduced to explicit overlap identities under the same
    /// provider-account lock. The linked reservation remains charged until
    /// exact-resource delete reconciliation proves absence.
    pub async fn reserve_lifetime_volume(
        &self,
        request: &CapacityReservationRequest,
        storage_resource_id: Uuid,
        configured_ceiling: u16,
        admission_headroom: u16,
        observed_provider_ids: &[String],
    ) -> Result<CapacityReservation> {
        if request.quantities.as_slice()
            != [CapacityQuantity {
                dimension: WorkspaceCapacityDimension::Volumes,
                quantity: 1,
            }]
            || configured_ceiling == 0
            || configured_ceiling > 100
            || admission_headroom >= configured_ceiling
        {
            return Err(MoaError::ValidationError(
                "lifetime volume reservation requires quantity one, a ceiling in 1..=100, and smaller headroom".to_string(),
            ));
        }
        let mut conn = self.begin().await?;
        lock_capacity_scopes(conn.as_mut(), request).await?;
        let fenced_resource = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM moa.sandbox_storage_resources AS resource
                JOIN moa.sandbox_workspace_operations AS operation
                  ON operation.operation_id = resource.create_operation_id
                 AND operation.tenant_id = resource.tenant_id
                WHERE resource.tenant_id = $1 AND resource.storage_resource_id = $2
                  AND resource.provider_account_id = $3
                  AND resource.provider_account_generation = $4
                  AND resource.lifecycle_state <> 'deleted'
                  AND operation.workspace_id = $5 AND operation.operation_id = $6
                  AND operation.expected_writer_epoch = $7
                  AND operation.expected_instance_generation = $8
                  AND operation.outcome_class = 'not_sent'
            )
            "#,
        )
        .bind(request.tenant_id)
        .bind(storage_resource_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.workspace_id)
        .bind(request.operation_id)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        if !fenced_resource {
            return Err(MoaError::StorageError(
                "lifetime reservation lost its exact storage-resource operation fence".to_string(),
            ));
        }

        let live_rows = sqlx::query(
            r#"
            SELECT storage_resource_id, provider_reference
            FROM moa.sandbox_storage_resources
            WHERE provider_account_id = $1 AND provider_account_generation = $2
              AND resource_kind = 'volume' AND lifecycle_state <> 'deleted'
            "#,
        )
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let mut resource_identities = HashMap::new();
        let mut identities = HashSet::new();
        for row in live_rows {
            let resource_id: Uuid = row.try_get("storage_resource_id").map_err(map_sqlx_error)?;
            let provider_reference: Option<String> =
                row.try_get("provider_reference").map_err(map_sqlx_error)?;
            let identity = provider_reference.map_or_else(
                || format!("storage:{resource_id}"),
                |provider_id| format!("provider:{provider_id}"),
            );
            resource_identities.insert(resource_id, identity.clone());
            identities.insert(identity);
        }
        for provider_id in observed_provider_ids {
            if provider_id.trim().is_empty() {
                return Err(MoaError::ValidationError(
                    "observed Daytona volume ids must be non-empty".to_string(),
                ));
            }
            identities.insert(format!("provider:{provider_id}"));
        }
        let pending = sqlx::query(
            r#"
            SELECT storage_resource_id, operation_id
            FROM moa.sandbox_capacity_reservations
            WHERE provider_account_id = $1 AND provider_account_generation = $2
              AND resource_dimension = 'volumes'
              AND reservation_state IN ('pending', 'committed', 'reconciling')
            "#,
        )
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        for row in pending {
            let resource_id: Option<Uuid> =
                row.try_get("storage_resource_id").map_err(map_sqlx_error)?;
            let operation_id: WorkspaceOperationId =
                row.try_get("operation_id").map_err(map_sqlx_error)?;
            identities.insert(
                resource_id
                    .and_then(|resource_id| resource_identities.get(&resource_id).cloned())
                    .unwrap_or_else(|| format!("operation:{operation_id}")),
            );
        }
        let effective = identities.len();
        if effective
            .checked_add(usize::from(admission_headroom))
            .is_none_or(|charged| charged > usize::from(configured_ceiling))
        {
            return Err(MoaError::ProviderError(format!(
                "Daytona volume capacity exhausted: effective={effective}, headroom={admission_headroom}, ceiling={configured_ceiling}"
            )));
        }

        let reservation_id = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO moa.sandbox_capacity_reservations (
                reservation_id, tenant_id, provider_account_id,
                provider_account_generation, workspace_id, operation_id,
                storage_resource_id, expected_writer_epoch,
                expected_instance_generation, resource_dimension, quantity
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'volumes', 1)
            ON CONFLICT (tenant_id, operation_id, resource_dimension)
                WHERE operation_id IS NOT NULL
            DO UPDATE
            SET updated_at = moa.sandbox_capacity_reservations.updated_at
            WHERE moa.sandbox_capacity_reservations.storage_resource_id = EXCLUDED.storage_resource_id
              AND moa.sandbox_capacity_reservations.provider_account_id = EXCLUDED.provider_account_id
              AND moa.sandbox_capacity_reservations.provider_account_generation = EXCLUDED.provider_account_generation
              AND moa.sandbox_capacity_reservations.expected_writer_epoch = EXCLUDED.expected_writer_epoch
              AND moa.sandbox_capacity_reservations.expected_instance_generation = EXCLUDED.expected_instance_generation
            RETURNING reservation_id
            "#,
        )
        .bind(reservation_id)
        .bind(request.tenant_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.workspace_id)
        .bind(request.operation_id)
        .bind(storage_resource_id)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError(
                "conflicting replay attempted to reuse a lifetime volume reservation".to_string(),
            )
        })?;
        let reservation_id = row.try_get("reservation_id").map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(CapacityReservation {
            reservation_id,
            dimension: WorkspaceCapacityDimension::Volumes,
            quantity: 1,
        })
    }

    /// Commits one linked lifetime reservation after the exact volume is verified.
    pub async fn commit_lifetime_volume(
        &self,
        request: &CapacityReservationRequest,
        storage_resource_id: Uuid,
    ) -> Result<bool> {
        let mut conn = self.begin().await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_capacity_reservations AS reservation
            SET reservation_state = 'committed', expires_at = NULL, updated_at = now()
            FROM moa.sandbox_storage_resources AS resource
            WHERE reservation.tenant_id = $1 AND reservation.operation_id = $2
              AND reservation.storage_resource_id = $3
              AND reservation.provider_account_id = $4
              AND reservation.provider_account_generation = $5
              AND reservation.expected_writer_epoch = $6
              AND reservation.expected_instance_generation = $7
              AND reservation.resource_dimension = 'volumes'
              AND reservation.reservation_state IN ('pending', 'reconciling')
              AND resource.tenant_id = reservation.tenant_id
              AND resource.storage_resource_id = reservation.storage_resource_id
              AND resource.create_operation_id = reservation.operation_id
              AND resource.lifecycle_state IN ('ready', 'attached')
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.operation_id)
        .bind(storage_resource_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        conn.commit().await?;
        Ok(changed)
    }

    /// Retains a linked volume charge while the provider outcome is ambiguous.
    pub async fn mark_lifetime_volume_reconciling(
        &self,
        request: &CapacityReservationRequest,
        storage_resource_id: Uuid,
    ) -> Result<bool> {
        let mut conn = self.begin().await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_capacity_reservations
            SET reservation_state = 'reconciling', expires_at = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2 AND storage_resource_id = $3
              AND provider_account_id = $4 AND provider_account_generation = $5
              AND expected_writer_epoch = $6 AND expected_instance_generation = $7
              AND resource_dimension = 'volumes'
              AND reservation_state IN ('pending', 'reconciling')
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.operation_id)
        .bind(storage_resource_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        conn.commit().await?;
        Ok(changed)
    }
}

/// Releases a lifetime workspace owner after exact deletion finalization.
pub(crate) async fn release_workspace_in_transaction(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    delete_generation: i64,
) -> Result<bool> {
    Ok(sqlx::query(
        r#"
        UPDATE moa.sandbox_capacity_reservations AS reservation
        SET reservation_state = 'released', updated_at = now()
        FROM moa.sandbox_workspaces AS workspace
        WHERE reservation.tenant_id = $1
          AND reservation.workspace_id = $2
          AND reservation.resource_dimension = 'workspaces'
          AND reservation.reservation_state = 'committed'
          AND reservation.expected_delete_generation + 1 = $3
          AND workspace.tenant_id = reservation.tenant_id
          AND workspace.workspace_id = reservation.workspace_id
          AND workspace.provider_account_id = reservation.provider_account_id
          AND workspace.provider_account_generation = reservation.provider_account_generation
          AND workspace.lifecycle_state = 'deleted'
          AND workspace.delete_generation = $3
        "#,
    )
    .bind(tenant_id)
    .bind(workspace_id)
    .bind(delete_generation)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected()
        == 1)
}

async fn lock_capacity_scopes(
    conn: &mut sqlx::PgConnection,
    request: &CapacityReservationRequest,
) -> Result<()> {
    lock_capacity_scope_values(conn, request.tenant_id, request.provider_account_id).await
}

async fn lock_capacity_scope_values(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    provider_account_id: ProviderAccountId,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("sandbox-capacity:tenant:{tenant_id}"))
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("sandbox-capacity:provider:{provider_account_id}"))
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

async fn load_operation_reservations(
    conn: &mut sqlx::PgConnection,
    request: &CapacityReservationRequest,
) -> Result<Vec<CapacityReservation>> {
    let rows = sqlx::query(
        r#"
        SELECT reservation_id, resource_dimension, quantity
        FROM moa.sandbox_capacity_reservations
        WHERE tenant_id = $1 AND workspace_id = $2 AND operation_id = $3
          AND provider_account_id = $4 AND provider_account_generation = $5
          AND expected_writer_epoch = $6 AND expected_instance_generation = $7
          AND reservation_state IN ('pending', 'committed', 'reconciling')
        ORDER BY resource_dimension
        "#,
    )
    .bind(request.tenant_id)
    .bind(request.workspace_id)
    .bind(request.operation_id)
    .bind(request.provider_account_id)
    .bind(request.provider_account_generation)
    .bind(request.expected_writer_epoch)
    .bind(request.expected_instance_generation)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx_error)?;
    rows.iter()
        .map(|row| {
            let quantity: i64 = row.try_get("quantity").map_err(map_sqlx_error)?;
            Ok(CapacityReservation {
                reservation_id: row.try_get("reservation_id").map_err(map_sqlx_error)?,
                dimension: WorkspaceCapacityDimension::from_label(
                    &row.try_get::<String, _>("resource_dimension")
                        .map_err(map_sqlx_error)?,
                )?,
                quantity: u64::try_from(quantity).map_err(|_| {
                    MoaError::StorageError(
                        "persisted capacity reservation quantity is not positive".to_string(),
                    )
                })?,
            })
        })
        .collect()
}

/// Reads the persisted state of one exact active-hand charge, when it exists.
async fn active_hand_reservation_state(
    conn: &mut sqlx::PgConnection,
    request: &ActiveHandCapacityRequest,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        r#"
        SELECT reservation_state
        FROM moa.sandbox_capacity_reservations
        WHERE tenant_id = $1 AND workspace_id = $2
          AND provider_account_id = $3 AND provider_account_generation = $4
          AND hand_provisioning_operation_id = $5
          AND hand_lease_generation = $6
          AND expected_writer_epoch = $7
          AND expected_instance_generation = $8
          AND resource_dimension = 'active_hands'
        "#,
    )
    .bind(request.tenant_id)
    .bind(request.workspace_id)
    .bind(request.provider_account_id)
    .bind(request.provider_account_generation)
    .bind(request.provisioning_operation_id)
    .bind(request.hand_lease_generation)
    .bind(request.expected_writer_epoch)
    .bind(request.expected_instance_generation)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)
}

async fn load_active_hand_reservation(
    conn: &mut sqlx::PgConnection,
    request: &ActiveHandCapacityRequest,
) -> Result<Option<CapacityReservation>> {
    let row = sqlx::query(
        r#"
        SELECT reservation_id, quantity
        FROM moa.sandbox_capacity_reservations
        WHERE tenant_id = $1 AND workspace_id = $2
          AND provider_account_id = $3 AND provider_account_generation = $4
          AND hand_provisioning_operation_id = $5
          AND hand_lease_generation = $6
          AND expected_writer_epoch = $7 AND expected_instance_generation = $8
          AND resource_dimension = 'active_hands' AND quantity = 1
          AND reservation_state IN ('pending', 'committed', 'reconciling')
        FOR UPDATE
        "#,
    )
    .bind(request.tenant_id)
    .bind(request.workspace_id)
    .bind(request.provider_account_id)
    .bind(request.provider_account_generation)
    .bind(request.provisioning_operation_id)
    .bind(request.hand_lease_generation)
    .bind(request.expected_writer_epoch)
    .bind(request.expected_instance_generation)
    .fetch_optional(conn)
    .await
    .map_err(map_sqlx_error)?;
    row.map(|row| {
        Ok(CapacityReservation {
            reservation_id: row.try_get("reservation_id").map_err(map_sqlx_error)?,
            dimension: WorkspaceCapacityDimension::ActiveHands,
            quantity: u64::try_from(row.try_get::<i64, _>("quantity").map_err(map_sqlx_error)?)
                .map_err(|_| {
                    MoaError::StorageError(
                        "persisted active-hand capacity quantity is invalid".to_string(),
                    )
                })?,
        })
    })
    .transpose()
}

fn validated_quantities(
    request: &CapacityReservationRequest,
) -> Result<BTreeMap<WorkspaceCapacityDimension, i64>> {
    if request.quantities.is_empty()
        || request.provider_account_generation <= 0
        || request.expected_writer_epoch < 0
        || request.expected_instance_generation < 0
    {
        return Err(MoaError::ValidationError(
            "capacity reservation requires quantities and valid generations".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    let mut quantities = BTreeMap::new();
    for item in &request.quantities {
        if item.quantity == 0
            || matches!(
                item.dimension,
                WorkspaceCapacityDimension::Workspaces | WorkspaceCapacityDimension::ActiveHands
            )
            || !seen.insert(item.dimension)
        {
            return Err(MoaError::ValidationError(
                "capacity quantities must be positive, operation-bound, and dimensions unique"
                    .to_string(),
            ));
        }
        let quantity = i64::try_from(item.quantity).map_err(|_| {
            MoaError::ValidationError(format!(
                "capacity quantity overflows postgres bigint for {}",
                item.dimension.as_str()
            ))
        })?;
        quantities.insert(item.dimension, quantity);
    }
    Ok(quantities)
}

fn validate_active_hand_request(request: &ActiveHandCapacityRequest) -> Result<()> {
    if request.provider_account_generation <= 0
        || request.hand_lease_generation <= 0
        || request.expected_writer_epoch < 0
        || request.expected_instance_generation < 0
    {
        return Err(MoaError::ValidationError(
            "active-hand capacity requires positive account/lease generations and valid workspace fences"
                .to_string(),
        ));
    }
    Ok(())
}

async fn enforce_capacity(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    provider_account_id: ProviderAccountId,
    provider_account_generation: i64,
    quantities: &BTreeMap<WorkspaceCapacityDimension, i64>,
) -> Result<()> {
    match capacity_shortfall(
        conn,
        tenant_id,
        provider_account_id,
        provider_account_generation,
        quantities,
    )
    .await?
    {
        None => Ok(()),
        Some(message) => Err(MoaError::ValidationError(message)),
    }
}

/// Reports the first exceeded limit instead of raising, for callers that treat
/// saturation as an ordinary decision rather than an admission fault.
///
/// Returns `None` when every requested dimension fits under both the tenant and
/// the provider-account ceiling. Genuine faults — a missing provider-account
/// generation, malformed limits, arithmetic overflow — are still errors.
async fn capacity_shortfall(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    provider_account_id: ProviderAccountId,
    provider_account_generation: i64,
    quantities: &BTreeMap<WorkspaceCapacityDimension, i64>,
) -> Result<Option<String>> {
    let provider_limits = sqlx::query(
        r#"
        SELECT configured_limits
        FROM moa.sandbox_provider_accounts
        WHERE provider_account_id = $1 AND generation = $2
        "#,
    )
    .bind(provider_account_id)
    .bind(provider_account_generation)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .ok_or_else(|| MoaError::StorageError("provider-account generation not found".to_string()))?
    .try_get::<Json<Value>, _>("configured_limits")
    .map_err(map_sqlx_error)?
    .0;
    let tenant_limits = sqlx::query(
        r#"
        SELECT configured_limits
        FROM moa.sandbox_tenant_capacity_limits
        WHERE tenant_id = $1
        FOR UPDATE
        "#,
    )
    .bind(tenant_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .map(|row| {
        row.try_get::<Json<Value>, _>("configured_limits")
            .map(|value| value.0)
            .map_err(map_sqlx_error)
    })
    .transpose()?
    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let tenant_limits = parse_limits(&tenant_limits, "tenant")?;
    let provider_limits = parse_limits(&provider_limits, "provider account")?;
    for (dimension, quantity) in quantities {
        let tenant_used = reserved_total(conn, Some(tenant_id), None, *dimension).await?;
        let provider_used =
            reserved_total(conn, None, Some(provider_account_id), *dimension).await?;
        if let Some(shortfall) = limit_shortfall(
            "tenant",
            *dimension,
            tenant_used,
            *quantity,
            tenant_limits.get(dimension).copied(),
        )? {
            return Ok(Some(shortfall));
        }
        if let Some(shortfall) = limit_shortfall(
            "provider account",
            *dimension,
            provider_used,
            *quantity,
            provider_limits.get(dimension).copied(),
        )? {
            return Ok(Some(shortfall));
        }
    }
    Ok(None)
}

/// Commits one exact active-hand reservation inside an existing transaction.
pub async fn commit_active_hand_in_transaction(
    conn: &mut sqlx::PgConnection,
    request: &ActiveHandCapacityRequest,
) -> Result<bool> {
    validate_active_hand_request(request)?;
    Ok(sqlx::query(
        r#"
        UPDATE moa.sandbox_capacity_reservations AS reservation
        SET reservation_state = 'committed', expires_at = NULL, updated_at = now()
        FROM moa.hand_leases AS lease
        WHERE reservation.tenant_id = $1
          AND reservation.workspace_id = $2
          AND reservation.provider_account_id = $3
          AND reservation.provider_account_generation = $4
          AND reservation.hand_provisioning_operation_id = $5
          AND reservation.hand_lease_generation = $6
          AND reservation.expected_writer_epoch = $7
          AND reservation.expected_instance_generation = $8
          AND reservation.resource_dimension = 'active_hands'
          AND reservation.reservation_state = 'pending'
          AND lease.tenant_id = reservation.tenant_id
          AND lease.provisioning_operation_id = reservation.hand_provisioning_operation_id
          AND lease.generation = reservation.hand_lease_generation
          AND lease.workspace_id = reservation.workspace_id
          AND lease.workspace_writer_epoch = reservation.expected_writer_epoch
          AND lease.workspace_instance_generation = reservation.expected_instance_generation
          AND lease.status = 'active'
          AND lease.handle IS NOT NULL
        "#,
    )
    .bind(request.tenant_id)
    .bind(request.workspace_id)
    .bind(request.provider_account_id)
    .bind(request.provider_account_generation)
    .bind(request.provisioning_operation_id)
    .bind(request.hand_lease_generation)
    .bind(request.expected_writer_epoch)
    .bind(request.expected_instance_generation)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected()
        == 1)
}

/// Releases the active-compute owner held by one exact live durable reaper claim.
///
/// The tri-state result distinguishes the recoverable crash window before the
/// reservation insert from a row whose identity matches but generation fences do
/// not. Reapers may accept `Missing` only when provider absence has already been
/// proven and the claimed lease never persisted a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveHandReaperRelease {
    /// The exact reservation was present and is now released.
    Released,
    /// No reservation exists for this provisioning identity.
    Missing,
    /// A reservation exists for the identity but carries different fences.
    Mismatched,
}

pub(crate) async fn release_active_hand_for_reaper_in_transaction(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    provisioning_operation_id: HandProvisioningOperationId,
    hand_lease_generation: i64,
    claim_token: Uuid,
) -> Result<ActiveHandReaperRelease> {
    let released = sqlx::query(
        r#"
        UPDATE moa.sandbox_capacity_reservations AS reservation
        SET reservation_state = 'released', updated_at = now()
        FROM moa.hand_leases AS lease
        JOIN moa.sandbox_workspaces AS workspace
          ON workspace.tenant_id = lease.tenant_id
         AND workspace.workspace_id = lease.workspace_id
        WHERE lease.tenant_id = $1
          AND lease.provisioning_operation_id = $2
          AND lease.generation = $3
          AND lease.status = 'reaping'
          AND lease.reap_claim_token = $4
          AND lease.reap_claim_expires_at > now()
          AND reservation.tenant_id = lease.tenant_id
          AND reservation.workspace_id = lease.workspace_id
          AND reservation.provider_account_id = workspace.provider_account_id
          AND reservation.provider_account_generation = workspace.provider_account_generation
          AND reservation.hand_provisioning_operation_id = lease.provisioning_operation_id
          AND reservation.hand_lease_generation = lease.generation
          AND reservation.expected_writer_epoch = lease.workspace_writer_epoch
          AND reservation.expected_instance_generation = lease.workspace_instance_generation
          AND reservation.resource_dimension = 'active_hands'
          AND reservation.reservation_state IN ('pending', 'committed', 'reconciling', 'released')
        "#,
    )
    .bind(tenant_id)
    .bind(provisioning_operation_id)
    .bind(hand_lease_generation)
    .bind(claim_token)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected()
        == 1;
    if released {
        return Ok(ActiveHandReaperRelease::Released);
    }
    let exists = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.sandbox_capacity_reservations
            WHERE tenant_id = $1
              AND hand_provisioning_operation_id = $2
              AND resource_dimension = 'active_hands'
        )
        "#,
    )
    .bind(tenant_id)
    .bind(provisioning_operation_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(if exists {
        ActiveHandReaperRelease::Mismatched
    } else {
        ActiveHandReaperRelease::Missing
    })
}

async fn release_active_hand_row(
    conn: &mut sqlx::PgConnection,
    request: &ActiveHandCapacityRequest,
) -> Result<bool> {
    Ok(sqlx::query(
        r#"
        UPDATE moa.sandbox_capacity_reservations
        SET reservation_state = 'released', updated_at = now()
        WHERE tenant_id = $1 AND workspace_id = $2
          AND provider_account_id = $3 AND provider_account_generation = $4
          AND hand_provisioning_operation_id = $5
          AND hand_lease_generation = $6
          AND expected_writer_epoch = $7
          AND expected_instance_generation = $8
          AND resource_dimension = 'active_hands'
          AND reservation_state IN ('pending', 'committed', 'reconciling')
        "#,
    )
    .bind(request.tenant_id)
    .bind(request.workspace_id)
    .bind(request.provider_account_id)
    .bind(request.provider_account_generation)
    .bind(request.provisioning_operation_id)
    .bind(request.hand_lease_generation)
    .bind(request.expected_writer_epoch)
    .bind(request.expected_instance_generation)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .rows_affected()
        == 1)
}

fn parse_limits(value: &Value, scope: &str) -> Result<BTreeMap<WorkspaceCapacityDimension, i64>> {
    let object = value.as_object().ok_or_else(|| {
        MoaError::StorageError(format!("{scope} capacity limits must be a JSON object"))
    })?;
    let mut limits = BTreeMap::new();
    for (label, value) in object {
        let dimension = WorkspaceCapacityDimension::from_label(label).map_err(|_| {
            MoaError::StorageError(format!("unknown {scope} capacity dimension: {label}"))
        })?;
        let limit = value.as_u64().ok_or_else(|| {
            MoaError::StorageError(format!(
                "{scope} capacity limit {label} must be a nonnegative integer"
            ))
        })?;
        let limit = i64::try_from(limit).map_err(|_| {
            MoaError::StorageError(format!("{scope} capacity limit {label} overflows bigint"))
        })?;
        limits.insert(dimension, limit);
    }
    Ok(limits)
}

async fn reserved_total(
    conn: &mut sqlx::PgConnection,
    tenant_id: Option<TenantId>,
    provider_account_id: Option<ProviderAccountId>,
    dimension: WorkspaceCapacityDimension,
) -> Result<i64> {
    sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(quantity), 0)::bigint
        FROM moa.sandbox_capacity_reservations
        WHERE ($1::uuid IS NULL OR tenant_id = $1) AND resource_dimension = $2
          AND reservation_state IN ('pending', 'committed', 'reconciling')
          AND ($3::uuid IS NULL OR provider_account_id = $3)
        "#,
    )
    .bind(tenant_id)
    .bind(dimension.as_str())
    .bind(provider_account_id)
    .fetch_one(conn)
    .await
    .map_err(map_sqlx_error)
}

fn limit_shortfall(
    scope: &str,
    dimension: WorkspaceCapacityDimension,
    used: i64,
    quantity: i64,
    limit: Option<i64>,
) -> Result<Option<String>> {
    let next = used.checked_add(quantity).ok_or_else(|| {
        MoaError::ValidationError(format!(
            "{scope} {} capacity arithmetic overflow",
            dimension.as_str()
        ))
    })?;
    if limit.is_some_and(|limit| next > limit) {
        // Both outcomes are recorded so the admitted/rejected ratio is meaningful; a
        // counter incremented only on rejection cannot distinguish a saturated fleet
        // from an idle one.
        record_workspace_quota_decision(dimension, SandboxWorkspaceQuotaDecision::Rejected);
        return Ok(Some(format!(
            "{scope} {} capacity exceeded: {used} + {quantity} > {}",
            dimension.as_str(),
            limit.unwrap_or_default()
        )));
    }
    record_workspace_quota_decision(dimension, SandboxWorkspaceQuotaDecision::Admitted);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use moa_core::types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceOperationId},
        sandbox_workspace::WorkspaceCapacityDimension,
    };

    use super::{CapacityQuantity, CapacityReservationRequest, validated_quantities};

    fn request(quantity: u64) -> CapacityReservationRequest {
        CapacityReservationRequest {
            tenant_id: TenantId::new(),
            workspace_id: SandboxWorkspaceId::new(),
            operation_id: WorkspaceOperationId::new(),
            provider_account_id: ProviderAccountId::new(),
            provider_account_generation: 1,
            expected_writer_epoch: 1,
            expected_instance_generation: 1,
            quantities: vec![CapacityQuantity {
                dimension: WorkspaceCapacityDimension::LogicalBytes,
                quantity,
            }],
        }
    }

    #[test]
    fn zero_and_bigint_overflow_are_rejected_before_database_io_offline() {
        // Pins: zero is never an unlimited sentinel and u64 cannot wrap BIGINT.
        assert!(validated_quantities(&request(0)).is_err());
        assert!(validated_quantities(&request(u64::MAX)).is_err());
        assert!(validated_quantities(&request(1)).is_ok());
    }

    #[test]
    fn lifetime_dimensions_require_their_generation_fenced_admission_paths_offline() {
        // Pins: generic operation capacity cannot bypass the workspace lifetime
        // or active-hand lease-generation admission contracts.
        for dimension in [
            WorkspaceCapacityDimension::Workspaces,
            WorkspaceCapacityDimension::ActiveHands,
        ] {
            let mut candidate = request(1);
            candidate.quantities[0].dimension = dimension;
            assert!(
                validated_quantities(&candidate).is_err(),
                "{dimension:?} must use its specialized fenced reservation path"
            );
        }
        let mut checkpoint = request(1);
        checkpoint.quantities[0].dimension = WorkspaceCapacityDimension::Checkpoints;
        assert!(
            validated_quantities(&checkpoint).is_ok(),
            "checkpoint capacity remains operation-bound"
        );
    }
}
