//! Atomic tenant and provider-account workspace-capacity admission.

use std::collections::{BTreeMap, HashMap, HashSet};

use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceOperationId},
        sandbox_workspace::WorkspaceCapacityDimension,
    },
};
use moa_db::ScopedConn;
use serde_json::Value;
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

use crate::core::leases::map_sqlx_error;

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

        let provider_limits = sqlx::query(
            r#"
            SELECT configured_limits
            FROM moa.sandbox_provider_accounts
            WHERE provider_account_id = $1 AND generation = $2
            "#,
        )
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .fetch_optional(conn.as_mut())
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
        .bind(request.tenant_id)
        .fetch_optional(conn.as_mut())
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

        for (dimension, quantity) in &quantities {
            let tenant_used =
                reserved_total(conn.as_mut(), Some(request.tenant_id), None, *dimension).await?;
            let provider_used = reserved_total(
                conn.as_mut(),
                None,
                Some(request.provider_account_id),
                *dimension,
            )
            .await?;
            enforce_limit(
                "tenant",
                *dimension,
                tenant_used,
                *quantity,
                tenant_limits.get(dimension).copied(),
            )?;
            enforce_limit(
                "provider account",
                *dimension,
                provider_used,
                *quantity,
                provider_limits.get(dimension).copied(),
            )?;
        }

        let mut reservations = Vec::with_capacity(quantities.len());
        for (dimension, quantity) in quantities {
            let reservation_id = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO moa.sandbox_capacity_reservations (
                    reservation_id, tenant_id, provider_account_id,
                    provider_account_generation, workspace_id, operation_id,
                    expected_writer_epoch, expected_instance_generation,
                    resource_dimension, quantity
                ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
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
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            reservations.push(CapacityReservation {
                reservation_id,
                dimension,
                quantity: u64::try_from(quantity).map_err(|_| {
                    MoaError::StorageError("negative persisted capacity quantity".to_string())
                })?,
            });
        }
        conn.commit().await?;
        Ok(reservations)
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
            ON CONFLICT (tenant_id, operation_id, resource_dimension) DO UPDATE
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

    /// Commits all checkpoint-count/logical-byte reservations for an exact operation.
    pub async fn commit_operation_reservations(
        &self,
        request: &CapacityReservationRequest,
    ) -> Result<u64> {
        let mut conn = self.begin().await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_capacity_reservations
            SET reservation_state = 'committed', expires_at = NULL, updated_at = now()
            WHERE tenant_id = $1 AND operation_id = $2
              AND provider_account_id = $3 AND provider_account_generation = $4
              AND expected_writer_epoch = $5 AND expected_instance_generation = $6
              AND resource_dimension IN ('checkpoints', 'logical_bytes')
              AND reservation_state = 'pending'
            "#,
        )
        .bind(request.tenant_id)
        .bind(request.operation_id)
        .bind(request.provider_account_id)
        .bind(request.provider_account_generation)
        .bind(request.expected_writer_epoch)
        .bind(request.expected_instance_generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        conn.commit().await?;
        Ok(changed)
    }
}

async fn lock_capacity_scopes(
    conn: &mut sqlx::PgConnection,
    request: &CapacityReservationRequest,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("sandbox-capacity:tenant:{}", request.tenant_id))
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "sandbox-capacity:provider:{}",
            request.provider_account_id
        ))
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
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
        if item.quantity == 0 || !seen.insert(item.dimension) {
            return Err(MoaError::ValidationError(
                "capacity quantities must be positive and dimensions unique".to_string(),
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

fn enforce_limit(
    scope: &str,
    dimension: WorkspaceCapacityDimension,
    used: i64,
    quantity: i64,
    limit: Option<i64>,
) -> Result<()> {
    let next = used.checked_add(quantity).ok_or_else(|| {
        MoaError::ValidationError(format!(
            "{scope} {} capacity arithmetic overflow",
            dimension.as_str()
        ))
    })?;
    if limit.is_some_and(|limit| next > limit) {
        return Err(MoaError::ValidationError(format!(
            "{scope} {} capacity exceeded: {used} + {quantity} > {}",
            dimension.as_str(),
            limit.unwrap_or_default()
        )));
    }
    Ok(())
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
}
