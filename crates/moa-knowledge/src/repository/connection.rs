//! Postgres knowledge connection persistence operations.

use super::row_mapping::*;
use super::*;

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
    .bind(&connection.provider)
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
    provider: &str,
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
    .bind(provider)
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
    provider: Option<&str>,
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
    .bind(provider)
    .bind(LIST_CONNECTIONS_LIMIT)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    rows.iter().map(connection_projection_from_row).collect()
}

pub(super) async fn record_provider_event(
    repository: &PostgresKnowledgeRepository,
    event: KnowledgeProviderEventRecord,
) -> Result<KnowledgeProviderEventRecord> {
    let mut conn = repository.begin().await?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO moa.knowledge_provider_events (
            provider_event_uid, tenant_id, storage_partition_id, connection_id,
            provider, provider_event_id, event_type, status, payload
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (tenant_id, provider, provider_event_id) DO NOTHING
        RETURNING provider_event_uid, tenant_id, connection_id, provider, provider_event_id,
                  event_type, status, payload, FALSE AS duplicate
        "#,
    )
    .bind(event.provider_event_uid)
    .bind(event.tenant_id.0)
    .bind(storage_partition_id(event.tenant_id))
    .bind(event.connection_uid)
    .bind(&event.provider)
    .bind(&event.provider_event_id)
    .bind(&event.event_type)
    .bind(&event.status)
    .bind(redact_provider_metadata(event.payload.clone()))
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    let row = match inserted {
        Some(row) => row,
        None => sqlx::query(
            r#"
                SELECT provider_event_uid, tenant_id, connection_id, provider,
                       provider_event_id, event_type, status, payload, TRUE AS duplicate
                FROM moa.knowledge_provider_events
                WHERE tenant_id = $1 AND provider = $2 AND provider_event_id = $3
                "#,
        )
        .bind(event.tenant_id.0)
        .bind(&event.provider)
        .bind(&event.provider_event_id)
        .fetch_one(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?,
    };
    conn.commit().await.map_err(map_moa_error)?;
    provider_event_from_row(&row)
}
