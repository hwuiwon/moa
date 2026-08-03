//! Postgres persistence for knowledge-connection remote-revocation progress.
//!
//! The row is the provider send ledger. Persisting `transmitting` is the
//! no-return boundary: a replay that observes it (or `unknown_outcome`) must not
//! issue another provider delete.

use super::row_mapping::*;
use super::*;

const DISCONNECT_COLUMNS: &str = r#"
    tenant_id, connection_uid, operation_id, request_hash,
    provider_operation_id, state, error_code, created_at, updated_at, completed_at
"#;

pub(super) async fn reserve_connection_disconnect(
    repository: &PostgresKnowledgeRepository,
    disconnect: NewKnowledgeConnectionDisconnect,
) -> Result<KnowledgeDisconnectReservation> {
    let mut conn = repository.begin().await?;
    let inserted = sqlx::query(&format!(
        r#"
        INSERT INTO moa.knowledge_connection_disconnect_progress (
            tenant_id, connection_uid, operation_id, request_hash,
            provider_operation_id, state
        )
        VALUES ($1, $2, $3, $4, $5, 'reserved')
        ON CONFLICT DO NOTHING
        RETURNING {DISCONNECT_COLUMNS}
        "#
    ))
    .bind(disconnect.tenant_id.0)
    .bind(disconnect.connection_uid)
    .bind(&disconnect.operation_id)
    .bind(&disconnect.request_hash)
    .bind(disconnect.provider_operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    if let Some(row) = inserted {
        let progress = connection_disconnect_from_row(&row)?;
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(KnowledgeDisconnectReservation::Reserved(progress));
    }

    // Read by either uniqueness boundary. A connection owns exactly one
    // disconnect row, while an operation id may never be reused for another
    // connection in the same tenant.
    let existing = sqlx::query(&format!(
        r#"
        SELECT {DISCONNECT_COLUMNS}
        FROM moa.knowledge_connection_disconnect_progress
        WHERE tenant_id = $1
          AND (connection_uid = $2 OR operation_id = $3)
        ORDER BY (connection_uid = $2) DESC
        LIMIT 1
        "#
    ))
    .bind(disconnect.tenant_id.0)
    .bind(disconnect.connection_uid)
    .bind(&disconnect.operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    let Some(row) = existing else {
        return Err(Error::Repository(
            "knowledge disconnect vanished between reservation and read".to_string(),
        ));
    };
    let existing = connection_disconnect_from_row(&row)?;
    if existing.connection_uid != disconnect.connection_uid
        || existing.request_hash != disconnect.request_hash
    {
        return Ok(KnowledgeDisconnectReservation::OperationConflict);
    }
    Ok(KnowledgeDisconnectReservation::Existing(existing))
}

pub(super) async fn advance_connection_disconnect(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
    transition: KnowledgeDisconnectTransition,
) -> Result<Option<KnowledgeConnectionDisconnectProgress>> {
    let target = transition.target_state();
    let mut conn = repository.begin().await?;
    let row = sqlx::query(&format!(
        r#"
        UPDATE moa.knowledge_connection_disconnect_progress
        SET state = $3,
            error_code = $4,
            updated_at = now(),
            completed_at = CASE WHEN $5 THEN now() ELSE NULL END
        WHERE tenant_id = $1
          AND connection_uid = $2
          AND state = $6
        RETURNING {DISCONNECT_COLUMNS}
        "#
    ))
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(target.as_str())
    .bind(transition.error_code())
    .bind(target.is_terminal())
    .bind(transition.source_state().as_str())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    row.as_ref().map(connection_disconnect_from_row).transpose()
}

pub(super) async fn get_connection_disconnect(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
) -> Result<Option<KnowledgeConnectionDisconnectProgress>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(&format!(
        r#"
        SELECT {DISCONNECT_COLUMNS}
        FROM moa.knowledge_connection_disconnect_progress
        WHERE tenant_id = $1 AND connection_uid = $2
        "#
    ))
    .bind(tenant_id.0)
    .bind(connection_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    row.as_ref().map(connection_disconnect_from_row).transpose()
}
