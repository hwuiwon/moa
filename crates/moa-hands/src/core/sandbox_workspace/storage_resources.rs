//! Tenant-scoped durable ownership for provider storage resources.

use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceOperationId},
        memory::RlsContext,
    },
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::core::leases::map_sqlx_error;

/// Stable provider storage lifecycle labels persisted by V58.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageResourceState {
    /// Create intent exists but no verified provider identifier is known.
    Creating,
    /// The exact provider resource was verified and is available.
    Ready,
    /// At least one verified compute attachment uses the resource.
    Attached,
    /// A fenced delete operation owns cleanup.
    Deleting,
    /// Provider absence has been durably proven.
    Deleted,
    /// The provider create/delete outcome is ambiguous.
    Unknown,
    /// The resource cannot be used but remains owned for cleanup.
    Failed,
}

impl StorageResourceState {
    /// Returns the exact V58 lifecycle label for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Attached => "attached",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }

    fn from_label(value: &str) -> Result<Self> {
        match value {
            "creating" => Ok(Self::Creating),
            "ready" => Ok(Self::Ready),
            "attached" => Ok(Self::Attached),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            "unknown" => Ok(Self::Unknown),
            "failed" => Ok(Self::Failed),
            other => Err(MoaError::StorageError(format!(
                "unknown sandbox storage-resource state: {other}"
            ))),
        }
    }
}

/// Fenced create intent persisted before a Daytona volume request.
#[derive(Debug, Clone)]
pub struct StorageResourceCreateIntent {
    /// Durable storage ownership row identity.
    pub storage_resource_id: Uuid,
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Workspace operation funding the first tenant-volume allocation.
    pub workspace_id: SandboxWorkspaceId,
    /// Exact create operation persisted before provider I/O.
    pub create_operation_id: WorkspaceOperationId,
    /// Provider account/isolation cell selected by admission.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account mapping generation.
    pub provider_account_generation: i64,
    /// Operator-authored isolation/security tier.
    pub security_class: String,
    /// Deterministic opaque provider create name.
    pub deterministic_name: String,
    /// Non-secret ownership fingerprint expected in provider inventory.
    pub verified_owner_fingerprint: String,
}

/// One exact tenant-owned provider storage resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageResource {
    /// Durable row identity.
    pub storage_resource_id: Uuid,
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Provider account/isolation cell.
    pub provider_account_id: ProviderAccountId,
    /// Exact provider-account mapping generation.
    pub provider_account_generation: i64,
    /// Operator-authored isolation/security tier.
    pub security_class: String,
    /// Deterministic opaque provider create name.
    pub deterministic_name: String,
    /// Exact opaque provider ID, once verified.
    pub provider_reference: Option<String>,
    /// Durable lifecycle state.
    pub state: StorageResourceState,
    /// Resource generation fencing create/delete callbacks.
    pub generation: i64,
    /// Operation that created the resource.
    pub create_operation_id: WorkspaceOperationId,
    /// Fenced delete owner, when deletion has begun.
    pub deletion_operation_id: Option<WorkspaceOperationId>,
    /// Non-secret ownership fingerprint verified against provider metadata.
    pub verified_owner_fingerprint: String,
}

/// Postgres storage-resource repository using forced tenant RLS.
#[derive(Clone)]
pub struct PostgresWorkspaceStorageResourceRepository {
    pool: PgPool,
}

impl PostgresWorkspaceStorageResourceRepository {
    /// Creates a repository over the runtime Postgres pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn begin(&self, tenant_id: TenantId) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(&self.pool, &RlsContext::tenant(tenant_id), true).await
    }

    /// Persists a deterministic create intent before `POST /volumes`.
    pub async fn persist_create_intent(
        &self,
        intent: &StorageResourceCreateIntent,
    ) -> Result<StorageResource> {
        validate_create_intent(intent)?;
        let mut conn = self.begin(intent.tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            INSERT INTO moa.sandbox_storage_resources (
                storage_resource_id, tenant_id, provider_account_id,
                provider_account_generation, resource_kind, security_class,
                deterministic_name, provider_reference, lifecycle_state,
                generation, create_operation_id, verified_owner_fingerprint
            )
            SELECT $1, operation.tenant_id, operation.provider_account_id,
                   operation.provider_account_generation, 'volume', $2, $3,
                   NULL, 'creating', 1, operation.operation_id, $4
            FROM moa.sandbox_workspace_operations AS operation
            WHERE operation.tenant_id = $5 AND operation.workspace_id = $6
              AND operation.operation_id = $7 AND operation.operation_kind = 'create'
              AND operation.provider_account_id = $8
              AND operation.provider_account_generation = $9
              AND operation.outcome_class = 'not_sent'
            ON CONFLICT (storage_resource_id) DO UPDATE
            SET updated_at = moa.sandbox_storage_resources.updated_at
            WHERE moa.sandbox_storage_resources.tenant_id = EXCLUDED.tenant_id
              AND moa.sandbox_storage_resources.provider_account_id = EXCLUDED.provider_account_id
              AND moa.sandbox_storage_resources.provider_account_generation = EXCLUDED.provider_account_generation
              AND moa.sandbox_storage_resources.security_class = EXCLUDED.security_class
              AND moa.sandbox_storage_resources.deterministic_name = EXCLUDED.deterministic_name
              AND moa.sandbox_storage_resources.create_operation_id = EXCLUDED.create_operation_id
            RETURNING {STORAGE_COLUMNS}
            "#,
        ))
        .bind(intent.storage_resource_id)
        .bind(intent.security_class.trim())
        .bind(intent.deterministic_name.trim())
        .bind(intent.verified_owner_fingerprint.trim())
        .bind(intent.tenant_id)
        .bind(intent.workspace_id)
        .bind(intent.create_operation_id)
        .bind(intent.provider_account_id)
        .bind(intent.provider_account_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError(
                "storage create intent lost its exact operation/account fence".to_string(),
            )
        })?;
        let resource = resource_from_row(&row)?;
        conn.commit().await?;
        Ok(resource)
    }

    /// Loads one resource under its immutable tenant owner.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        storage_resource_id: Uuid,
    ) -> Result<Option<StorageResource>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            "SELECT {STORAGE_COLUMNS} FROM moa.sandbox_storage_resources WHERE tenant_id = $1 AND storage_resource_id = $2"
        ))
        .bind(tenant_id)
        .bind(storage_resource_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let resource = row.as_ref().map(resource_from_row).transpose()?;
        conn.commit().await?;
        Ok(resource)
    }

    /// Resolves the sole live tenant volume for one account generation/security class.
    pub async fn live_tenant_volume(
        &self,
        tenant_id: TenantId,
        provider_account_id: ProviderAccountId,
        provider_account_generation: i64,
        security_class: &str,
    ) -> Result<Option<StorageResource>> {
        if security_class.trim().is_empty() || provider_account_generation <= 0 {
            return Err(MoaError::ValidationError(
                "tenant-volume lookup requires a security class and account generation".to_string(),
            ));
        }
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {STORAGE_COLUMNS}
            FROM moa.sandbox_storage_resources
            WHERE tenant_id = $1 AND provider_account_id = $2
              AND provider_account_generation = $3 AND security_class = $4
              AND resource_kind = 'volume' AND lifecycle_state <> 'deleted'
            "#,
        ))
        .bind(tenant_id)
        .bind(provider_account_id)
        .bind(provider_account_generation)
        .bind(security_class.trim())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let resource = row.as_ref().map(resource_from_row).transpose()?;
        conn.commit().await?;
        Ok(resource)
    }

    /// Loads one exact provider resource under tenant and account-generation fences.
    pub async fn by_provider_reference(
        &self,
        tenant_id: TenantId,
        provider_account_id: ProviderAccountId,
        provider_account_generation: i64,
        provider_reference: &str,
    ) -> Result<Option<StorageResource>> {
        if provider_reference.trim().is_empty() || provider_account_generation <= 0 {
            return Err(MoaError::ValidationError(
                "provider-resource lookup requires an exact reference and generation".to_string(),
            ));
        }
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query(&format!(
            r#"
            SELECT {STORAGE_COLUMNS}
            FROM moa.sandbox_storage_resources
            WHERE tenant_id = $1 AND provider_account_id = $2
              AND provider_account_generation = $3 AND provider_reference = $4
              AND lifecycle_state <> 'deleted'
            "#,
        ))
        .bind(tenant_id)
        .bind(provider_account_id)
        .bind(provider_account_generation)
        .bind(provider_reference.trim())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let resource = row.as_ref().map(resource_from_row).transpose()?;
        conn.commit().await?;
        Ok(resource)
    }

    /// Records the exact provider ID learned for a fenced create generation.
    pub async fn confirm_created(
        &self,
        tenant_id: TenantId,
        storage_resource_id: Uuid,
        generation: i64,
        create_operation_id: WorkspaceOperationId,
        provider_reference: &str,
    ) -> Result<bool> {
        if provider_reference.trim().is_empty() || generation <= 0 {
            return Err(MoaError::ValidationError(
                "confirmed storage requires a provider reference and positive generation"
                    .to_string(),
            ));
        }
        let mut conn = self.begin(tenant_id).await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_storage_resources
            SET provider_reference = $5, lifecycle_state = 'ready', updated_at = now()
            WHERE tenant_id = $1 AND storage_resource_id = $2 AND generation = $3
              AND create_operation_id = $4 AND lifecycle_state IN ('creating', 'unknown')
              AND (provider_reference IS NULL OR provider_reference = $5)
            "#,
        )
        .bind(tenant_id)
        .bind(storage_resource_id)
        .bind(generation)
        .bind(create_operation_id)
        .bind(provider_reference.trim())
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        conn.commit().await?;
        Ok(changed)
    }

    /// Retains ownership while marking an ambiguous create/delete outcome.
    pub async fn mark_unknown(
        &self,
        tenant_id: TenantId,
        storage_resource_id: Uuid,
        generation: i64,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let changed = sqlx::query(
            "UPDATE moa.sandbox_storage_resources SET lifecycle_state = 'unknown', updated_at = now() WHERE tenant_id = $1 AND storage_resource_id = $2 AND generation = $3 AND lifecycle_state <> 'deleted'",
        )
        .bind(tenant_id)
        .bind(storage_resource_id)
        .bind(generation)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        conn.commit().await?;
        Ok(changed)
    }

    /// Fences deletion to one exact resource generation and durable delete operation.
    pub async fn begin_delete(
        &self,
        tenant_id: TenantId,
        storage_resource_id: Uuid,
        generation: i64,
        delete_operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_storage_resources AS resource
            SET lifecycle_state = 'deleting', deletion_operation_id = operation.operation_id,
                updated_at = now()
            FROM moa.sandbox_workspace_operations AS operation
            WHERE resource.tenant_id = $1 AND resource.storage_resource_id = $2
              AND resource.generation = $3 AND resource.lifecycle_state IN ('ready', 'unknown', 'failed')
              AND operation.tenant_id = resource.tenant_id AND operation.operation_id = $4
              AND operation.operation_kind = 'delete'
              AND operation.provider_account_id = resource.provider_account_id
              AND operation.provider_account_generation = resource.provider_account_generation
            "#,
        )
        .bind(tenant_id)
        .bind(storage_resource_id)
        .bind(generation)
        .bind(delete_operation_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        conn.commit().await?;
        Ok(changed)
    }

    /// Confirms exact-resource absence and releases only its lifetime reservation.
    pub async fn confirm_deleted_and_release_lifetime(
        &self,
        tenant_id: TenantId,
        storage_resource_id: Uuid,
        generation: i64,
        delete_operation_id: WorkspaceOperationId,
    ) -> Result<bool> {
        let mut conn = self.begin(tenant_id).await?;
        let changed = sqlx::query(
            r#"
            UPDATE moa.sandbox_storage_resources AS resource
            SET lifecycle_state = 'deleted', provider_reference = NULL, updated_at = now()
            FROM moa.sandbox_workspace_operations AS operation
            WHERE resource.tenant_id = $1 AND resource.storage_resource_id = $2
              AND resource.generation = $3 AND resource.lifecycle_state = 'deleting'
              AND resource.deletion_operation_id = $4
              AND operation.tenant_id = resource.tenant_id
              AND operation.operation_id = resource.deletion_operation_id
              AND operation.outcome_class = 'confirmed'
              AND operation.confirmed_disposition = 'resource_absent'
              AND operation.absence_observation_count = 2
            "#,
        )
        .bind(tenant_id)
        .bind(storage_resource_id)
        .bind(generation)
        .bind(delete_operation_id)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        .rows_affected()
            == 1;
        if changed {
            sqlx::query(
                r#"
                UPDATE moa.sandbox_capacity_reservations
                SET reservation_state = 'released', updated_at = now()
                WHERE tenant_id = $1 AND storage_resource_id = $2
                  AND resource_dimension = 'volumes'
                  AND reservation_state IN ('committed', 'reconciling')
                "#,
            )
            .bind(tenant_id)
            .bind(storage_resource_id)
            .execute(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        }
        conn.commit().await?;
        Ok(changed)
    }
}

fn validate_create_intent(intent: &StorageResourceCreateIntent) -> Result<()> {
    if intent.provider_account_generation <= 0
        || intent.security_class.trim().is_empty()
        || intent.deterministic_name.trim().is_empty()
        || intent.verified_owner_fingerprint.trim().is_empty()
    {
        return Err(MoaError::ValidationError(
            "storage create intent requires an account generation, security class, deterministic name, and owner fingerprint".to_string(),
        ));
    }
    Ok(())
}

const STORAGE_COLUMNS: &str = r#"
storage_resource_id, tenant_id, provider_account_id, provider_account_generation,
security_class, deterministic_name, provider_reference, lifecycle_state, generation,
create_operation_id, deletion_operation_id, verified_owner_fingerprint
"#;

fn resource_from_row(row: &sqlx::postgres::PgRow) -> Result<StorageResource> {
    Ok(StorageResource {
        storage_resource_id: row.try_get("storage_resource_id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        provider_account_id: row.try_get("provider_account_id").map_err(map_sqlx_error)?,
        provider_account_generation: row
            .try_get("provider_account_generation")
            .map_err(map_sqlx_error)?,
        security_class: row.try_get("security_class").map_err(map_sqlx_error)?,
        deterministic_name: row.try_get("deterministic_name").map_err(map_sqlx_error)?,
        provider_reference: row.try_get("provider_reference").map_err(map_sqlx_error)?,
        state: StorageResourceState::from_label(
            row.try_get::<String, _>("lifecycle_state")
                .map_err(map_sqlx_error)?
                .as_str(),
        )?,
        generation: row.try_get("generation").map_err(map_sqlx_error)?,
        create_operation_id: row.try_get("create_operation_id").map_err(map_sqlx_error)?,
        deletion_operation_id: row
            .try_get("deletion_operation_id")
            .map_err(map_sqlx_error)?,
        verified_owner_fingerprint: row
            .try_get("verified_owner_fingerprint")
            .map_err(map_sqlx_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::StorageResourceState;

    #[test]
    fn storage_resource_state_labels_are_exact_offline() {
        // Pins: repository state labels stay identical to the V58 constraint.
        for (state, label) in [
            (StorageResourceState::Creating, "creating"),
            (StorageResourceState::Ready, "ready"),
            (StorageResourceState::Attached, "attached"),
            (StorageResourceState::Deleting, "deleting"),
            (StorageResourceState::Deleted, "deleted"),
            (StorageResourceState::Unknown, "unknown"),
            (StorageResourceState::Failed, "failed"),
        ] {
            assert_eq!(state.as_str(), label);
            assert_eq!(
                StorageResourceState::from_label(label).expect("known state should parse"),
                state
            );
        }
        assert!(StorageResourceState::from_label("gone").is_err());
    }
}
