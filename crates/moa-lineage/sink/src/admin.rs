//! Admin read helpers for hot lineage rows.

use moa_core::wire::LineageRecordView;
use moa_core::{SessionId, TenantId, UserId, WorkspaceId};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Loads lineage records for one workspace-scoped session or turn id.
pub async fn explain_records(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    id: Uuid,
) -> Result<Vec<LineageRecordView>> {
    let rows = sqlx::query(
        r#"
        SELECT turn_id, session_id, user_id, workspace_id, ts, record_kind, payload
        FROM analytics.turn_lineage
        WHERE workspace_id = $1 AND (session_id = $2 OR turn_id = $2)
        ORDER BY ts ASC, record_kind ASC
        "#,
    )
    .bind(workspace_id.as_str())
    .bind(id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(lineage_record_from_row).collect()
}

/// Executes a service-vetted lineage query inside a caller-owned read-only transaction.
pub async fn execute_prepared_lineage_query(
    conn: &mut PgConnection,
    prepared_sql: &str,
    workspace_id: &WorkspaceId,
    since: &str,
) -> Result<Value> {
    let rows = sqlx::query_scalar(&format!(
        "SELECT COALESCE(jsonb_agg(row_to_json(lineage_query)), '[]'::jsonb) \
         FROM ({prepared_sql}) lineage_query"
    ))
    .bind(workspace_id.as_str())
    .bind(since)
    .fetch_one(conn)
    .await?;
    Ok(rows)
}

/// Loads hot lineage rows matching a subject for DSAR export.
pub async fn load_dsar_export_records(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    subject: &str,
) -> Result<Vec<Value>> {
    let pattern = format!("%{subject}%");
    let records = sqlx::query_scalar(
        r#"
        SELECT row_to_json(lineage_row)::jsonb
        FROM (
            SELECT turn_id, session_id, user_id, workspace_id, ts, record_kind, payload,
                   integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE workspace_id = $1 AND payload::text ILIKE $2
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            LIMIT 10000
        ) lineage_row
        "#,
    )
    .bind(workspace_id.as_str())
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(records)
}

fn lineage_record_from_row(row: sqlx::postgres::PgRow) -> Result<LineageRecordView> {
    let session_id: Uuid = row.try_get("session_id")?;
    let user_id: String = row.try_get("user_id")?;
    let workspace_id: String = row.try_get("workspace_id")?;
    let tenant_id = Uuid::parse_str(&workspace_id)
        .map(TenantId::from)
        .map_err(|error| {
            Error::Invalid(format!("lineage workspace_id is not a tenant id: {error}"))
        })?;
    Ok(LineageRecordView {
        turn_id: row.try_get("turn_id")?,
        session_id: Some(SessionId(session_id)),
        tenant_id: Some(tenant_id),
        user_id: Some(UserId::new(user_id)),
        ts: row.try_get("ts")?,
        record_kind: row.try_get("record_kind")?,
        payload: row.try_get("payload")?,
        summary: None,
    })
}
