//! Postgres knowledge connection persistence operations.

use super::row_mapping::*;
use super::*;

/// Persistence operations for knowledge connection projections and link ledgers.
#[async_trait]
pub trait KnowledgeConnectionRepository: Send + Sync {
    /// Saves or updates a linked connection.
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection>;

    /// Gets a linked connection by identifier.
    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>>;

    /// Deletes a newly-created knowledge projection while compensating a failed link.
    async fn delete_connection_projection(&self, connection_uid: Uuid) -> Result<bool>;

    /// Advances one active connection's successful sync watermark.
    async fn mark_connection_synced(
        &self,
        connection_uid: Uuid,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Gets the connection an upsert of this provider account would replace.
    async fn connection_by_provider_account(
        &self,
        provider: LinkedProviderKind,
        connector: &str,
        provider_account_id: &str,
    ) -> Result<Option<KnowledgeConnection>>;

    /// Reserves the operation-fenced claim that owns one link.
    async fn reserve_link_claim(&self, claim: NewLinkClaim) -> Result<LinkClaimReservation>;

    /// Advances one link claim by compare-and-swap.
    async fn advance_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        transition: LinkClaimTransition,
    ) -> Result<Option<LinkClaim>>;

    /// Reads one link claim.
    async fn get_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<Option<LinkClaim>>;

    /// Reserves the one remote-revocation operation for a connection.
    async fn reserve_connection_disconnect(
        &self,
        disconnect: NewKnowledgeConnectionDisconnect,
    ) -> Result<KnowledgeDisconnectReservation>;

    /// Advances one remote-revocation ledger row by compare-and-swap.
    async fn advance_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        transition: KnowledgeDisconnectTransition,
    ) -> Result<Option<KnowledgeConnectionDisconnectProgress>>;

    /// Reads the durable remote-revocation progress for one connection.
    async fn get_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> Result<Option<KnowledgeConnectionDisconnectProgress>>;

    /// Removes at most `limit` link claims for this repository's tenant.
    async fn purge_tenant_link_claims(&self, limit: u32) -> Result<u64>;

    /// Updates provider-native selected source state and clears the sync watermark.
    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection>;

    /// Lists linked-connection projections for a tenant.
    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<LinkedProviderKind>,
    ) -> Result<Vec<KnowledgeConnectionProjection>>;
}

pub(super) async fn upsert_connection(
    repository: &PostgresKnowledgeRepository,
    connection: KnowledgeConnection,
) -> Result<KnowledgeConnection> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections (
            connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
            provider_connection_id, connector, metadata, source_selection, information_barrier,
            created_at, updated_at, last_synced_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $5, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (tenant_id, provider, provider_config_key, provider_connection_id)
        DO UPDATE SET
            metadata = EXCLUDED.metadata,
            source_selection = EXCLUDED.source_selection,
            information_barrier = EXCLUDED.information_barrier,
            last_synced_at = EXCLUDED.last_synced_at,
            updated_at = EXCLUDED.updated_at
        RETURNING connection_uid, tenant_id, provider, connector, provider_connection_id,
                  metadata, source_selection, information_barrier, created_at, updated_at,
                  last_synced_at
        "#,
    )
    .bind(connection.connection_uid)
    .bind(connection.tenant_id.0)
    .bind(storage_partition_id(connection.tenant_id))
    .bind(connection.provider.as_str())
    .bind(&connection.connector)
    .bind(&connection.provider_account_id)
    .bind(redact_provider_metadata(connection.metadata))
    .bind(normalize_source_selection(connection.source_selection))
    .bind(
        connection
            .information_barrier
            .as_ref()
            .map(InformationBarrierId::as_str),
    )
    .bind(connection.created_at)
    .bind(connection.updated_at)
    .bind(connection.last_synced_at)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    connection_from_row(&row)
}

pub(super) async fn get_connection(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
) -> Result<Option<KnowledgeConnection>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT connection_uid, tenant_id, provider, connector, provider_connection_id,
               metadata, source_selection, information_barrier, created_at, updated_at,
               last_synced_at
        FROM moa.knowledge_connections
        WHERE connection_uid = $1
        "#,
    )
    .bind(connection_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(connection_from_row).transpose()
}

pub(super) async fn delete_connection_projection(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
) -> Result<bool> {
    let mut conn = repository.begin().await?;
    let removed = sqlx::query(
        r#"
        DELETE FROM moa.knowledge_connections
        WHERE tenant_id = $1 AND connection_uid = $2
        "#,
    )
    .bind(repository.scoped_tenant_id().0)
    .bind(connection_uid)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?
    .rows_affected();
    conn.commit().await.map_err(map_moa_error)?;
    Ok(removed == 1)
}

pub(super) async fn mark_connection_synced(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
    completed_at: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE moa.knowledge_connections AS connection
        SET last_synced_at = $3, updated_at = NOW()
        FROM moa.connector_connections AS parent
        WHERE connection.tenant_id = $1
          AND connection.connection_uid = $2
          AND parent.connection_uid = connection.connection_uid
          AND parent.tenant_id = connection.tenant_id
          AND parent.lifecycle_status IN ('active', 'suspended')
        "#,
    )
    .bind(repository.scoped_tenant_id().0)
    .bind(connection_uid)
    .bind(completed_at)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    if updated.rows_affected() != 1 {
        return Err(Error::Repository(
            "knowledge connection parent was not visible for sync completion".to_string(),
        ));
    }
    conn.commit().await.map_err(map_moa_error)?;
    Ok(())
}

pub(super) async fn connection_by_provider_account(
    repository: &PostgresKnowledgeRepository,
    provider: LinkedProviderKind,
    connector: &str,
    provider_account_id: &str,
) -> Result<Option<KnowledgeConnection>> {
    let mut conn = repository.begin().await?;
    // Exactly `upsert_connection`'s conflict target, so the answer is what the
    // upsert will do rather than a similar-looking lookup that could diverge.
    let row = sqlx::query(
        r#"
        SELECT connection_uid, tenant_id, provider, connector, provider_connection_id,
               metadata, source_selection, information_barrier, created_at, updated_at,
               last_synced_at
        FROM moa.knowledge_connections
        WHERE tenant_id = $1
          AND provider = $2
          AND provider_config_key = $3
          AND provider_connection_id = $4
        "#,
    )
    .bind(repository.scoped_tenant_id().0)
    .bind(provider.as_str())
    .bind(connector)
    .bind(provider_account_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(connection_from_row).transpose()
}

pub(super) async fn update_connection_source_selection(
    repository: &PostgresKnowledgeRepository,
    connection_uid: Uuid,
    source_selection: serde_json::Value,
) -> Result<KnowledgeConnection> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(
        r#"
        UPDATE moa.knowledge_connections AS connection
        SET source_selection = $2,
            last_synced_at = NULL,
            updated_at = now()
        FROM moa.connector_connections AS parent
        WHERE connection.connection_uid = $1
          AND parent.connection_uid = connection.connection_uid
          AND parent.tenant_id = connection.tenant_id
          AND parent.lifecycle_status = 'active'
        RETURNING connection.connection_uid, connection.tenant_id, connection.provider,
                  connection.connector, connection.provider_connection_id,
                  connection.metadata, connection.source_selection, connection.information_barrier,
                  connection.created_at, connection.updated_at, connection.last_synced_at
        "#,
    )
    .bind(connection_uid)
    .bind(normalize_source_selection(source_selection))
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        return Err(Error::Repository(
            "knowledge connection parent was not visible and active for source selection update"
                .to_string(),
        ));
    };
    conn.commit().await.map_err(map_moa_error)?;
    connection_from_row(&row)
}

pub(super) async fn list_connections(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    provider: Option<LinkedProviderKind>,
) -> Result<Vec<KnowledgeConnectionProjection>> {
    let mut conn = repository.begin().await?;
    let rows = sqlx::query(
        r#"
        SELECT c.connection_uid, c.tenant_id, c.provider, c.connector,
               c.provider_connection_id, c.metadata, c.source_selection, c.information_barrier,
               c.created_at, c.updated_at, c.last_synced_at,
               parent.lifecycle_status AS parent_lifecycle_status,
               latest.status AS last_sync_status
        FROM moa.knowledge_connections c
        JOIN moa.connector_connections parent
          ON parent.connection_uid = c.connection_uid
         AND parent.tenant_id = c.tenant_id
        LEFT JOIN LATERAL (
            SELECT status
            FROM moa.knowledge_sync_runs
            WHERE connection_id = c.connection_uid
            ORDER BY started_at DESC, sync_run_uid DESC
            LIMIT 1
        ) latest ON TRUE
        WHERE c.tenant_id = $1
          AND ($2::TEXT IS NULL OR c.provider = $2)
        ORDER BY c.updated_at DESC, c.connection_uid DESC
        LIMIT $3
        "#,
    )
    .bind(tenant_id.0)
    .bind(provider.map(LinkedProviderKind::as_str))
    .bind(LIST_CONNECTIONS_LIMIT)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(connection_projection_from_row).collect()
}

#[async_trait]
impl KnowledgeConnectionRepository for PostgresKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> Result<KnowledgeConnection> {
        upsert_connection(self, connection).await
    }

    async fn get_connection(&self, connection_uid: Uuid) -> Result<Option<KnowledgeConnection>> {
        get_connection(self, connection_uid).await
    }

    async fn delete_connection_projection(&self, connection_uid: Uuid) -> Result<bool> {
        delete_connection_projection(self, connection_uid).await
    }

    async fn mark_connection_synced(
        &self,
        connection_uid: Uuid,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        mark_connection_synced(self, connection_uid, completed_at).await
    }

    async fn connection_by_provider_account(
        &self,
        provider: LinkedProviderKind,
        connector: &str,
        provider_account_id: &str,
    ) -> Result<Option<KnowledgeConnection>> {
        connection_by_provider_account(self, provider, connector, provider_account_id).await
    }

    async fn reserve_link_claim(&self, claim: NewLinkClaim) -> Result<LinkClaimReservation> {
        super::link_claim::reserve_link_claim(self, claim).await
    }

    async fn advance_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
        transition: LinkClaimTransition,
    ) -> Result<Option<LinkClaim>> {
        super::link_claim::advance_link_claim(self, tenant_id, operation_id, transition).await
    }

    async fn get_link_claim(
        &self,
        tenant_id: TenantId,
        operation_id: &str,
    ) -> Result<Option<LinkClaim>> {
        super::link_claim::get_link_claim(self, tenant_id, operation_id).await
    }

    async fn reserve_connection_disconnect(
        &self,
        disconnect: NewKnowledgeConnectionDisconnect,
    ) -> Result<KnowledgeDisconnectReservation> {
        super::disconnect::reserve_connection_disconnect(self, disconnect).await
    }

    async fn advance_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        transition: KnowledgeDisconnectTransition,
    ) -> Result<Option<KnowledgeConnectionDisconnectProgress>> {
        super::disconnect::advance_connection_disconnect(
            self,
            tenant_id,
            connection_uid,
            transition,
        )
        .await
    }

    async fn get_connection_disconnect(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> Result<Option<KnowledgeConnectionDisconnectProgress>> {
        super::disconnect::get_connection_disconnect(self, tenant_id, connection_uid).await
    }

    async fn purge_tenant_link_claims(&self, limit: u32) -> Result<u64> {
        super::link_claim::purge_tenant_link_claims(self, limit).await
    }

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: serde_json::Value,
    ) -> Result<KnowledgeConnection> {
        update_connection_source_selection(self, connection_uid, source_selection).await
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<LinkedProviderKind>,
    ) -> Result<Vec<KnowledgeConnectionProjection>> {
        list_connections(self, tenant_id, provider).await
    }
}
