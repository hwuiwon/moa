//! Edge-row helpers used by graph write transaction entry points.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::json;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{
    Error, PostgresGraphStore, Result,
    changelog::ChangelogRecord,
    edge::{EdgeLabel, EdgeWriteIntent},
};

use super::{
    node::{StoredNode, fetch_stored_node, stored_node_from_row},
    scope::{ensure_same_scope, runtime_ids_from_parts, validate_scope_shape},
};

/// One edge intent paired with the exact runtime scope written to Postgres.
pub(super) struct ScopedEdgeWrite {
    pub(super) intent: EdgeWriteIntent,
    pub(super) tenant_id: Uuid,
    pub(super) contact_id: Option<Uuid>,
}

/// Builds the changelog projection for one inserted edge.
pub(super) fn edge_changelog(intent: &EdgeWriteIntent) -> ChangelogRecord {
    ChangelogRecord {
        storage_partition_id: intent.storage_partition_id.clone(),
        contact_id: intent.contact_id.clone(),
        scope: intent.scope.clone(),
        actor_id: Some(intent.actor_id.clone()),
        actor_kind: intent.actor_kind.clone(),
        op: "create".to_string(),
        target_kind: "edge".to_string(),
        target_label: intent.label.as_str().to_string(),
        target_uid: intent.uid,
        payload: json!({
            "after": intent.properties,
            "start_uid": intent.start_uid,
            "end_uid": intent.end_uid,
        }),
        redaction_marker: None,
        pii_class: "none".to_string(),
        audit_metadata: None,
        cause_change_id: None,
    }
}

/// Inserts a validated edge batch and returns only newly created UIDs in input order.
pub(super) async fn insert_edge_index_batch(
    conn: &mut PgConnection,
    writes: &[ScopedEdgeWrite],
) -> Result<Vec<Uuid>> {
    let uids = writes
        .iter()
        .map(|write| write.intent.uid)
        .collect::<Vec<_>>();
    let labels = writes
        .iter()
        .map(|write| write.intent.label.as_str())
        .collect::<Vec<_>>();
    let start_uids = writes
        .iter()
        .map(|write| write.intent.start_uid)
        .collect::<Vec<_>>();
    let end_uids = writes
        .iter()
        .map(|write| write.intent.end_uid)
        .collect::<Vec<_>>();
    let storage_partition_ids = writes
        .iter()
        .map(|write| write.intent.storage_partition_id.as_deref())
        .collect::<Vec<_>>();
    let user_ids = writes
        .iter()
        .map(|write| write.intent.contact_id.as_deref())
        .collect::<Vec<_>>();
    let tenant_ids = writes
        .iter()
        .map(|write| write.tenant_id)
        .collect::<Vec<_>>();
    let contact_ids = writes
        .iter()
        .map(|write| write.contact_id)
        .collect::<Vec<_>>();
    let valid_froms = writes
        .iter()
        .map(|write| write.intent.valid_from)
        .collect::<Vec<_>>();
    let properties = writes
        .iter()
        .map(|write| serde_json::to_string(&write.intent.properties))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(sqlx::query_scalar::<_, Uuid>(
        r#"
        WITH input AS (
            SELECT row.uid, row.label, row.start_uid, row.end_uid,
                   row.storage_partition_id, row.user_id, row.tenant_id,
                   row.contact_id, row.valid_from, row.properties::JSONB,
                   row.input_ordinal
            FROM UNNEST(
                $1::UUID[], $2::TEXT[], $3::UUID[], $4::UUID[], $5::TEXT[],
                $6::TEXT[], $7::UUID[], $8::UUID[], $9::TIMESTAMPTZ[], $10::TEXT[]
            ) WITH ORDINALITY AS row(
                uid, label, start_uid, end_uid, storage_partition_id, user_id,
                tenant_id, contact_id, valid_from, properties, input_ordinal
            )
        ), inserted AS (
            INSERT INTO moa.edge_index
                (uid, label, start_uid, end_uid, storage_partition_id, user_id,
                 tenant_id, contact_id, valid_from, properties)
            SELECT uid, label, start_uid, end_uid, storage_partition_id, user_id,
                   tenant_id, contact_id, valid_from, properties
            FROM input
            ORDER BY uid
            ON CONFLICT (uid) DO NOTHING
            RETURNING uid
        )
        SELECT input.uid
        FROM input
        JOIN inserted USING (uid)
        ORDER BY input.input_ordinal
        "#,
    )
    .bind(&uids)
    .bind(&labels)
    .bind(&start_uids)
    .bind(&end_uids)
    .bind(&storage_partition_ids)
    .bind(&user_ids)
    .bind(&tenant_ids)
    .bind(&contact_ids)
    .bind(&valid_froms)
    .bind(&properties)
    .fetch_all(conn)
    .await?)
}

/// Returns whether an edge UID already exists inside the caller's transaction.
pub(super) async fn edge_exists(conn: &mut PgConnection, uid: Uuid) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM moa.edge_index WHERE uid = $1)")
        .bind(uid)
        .fetch_one(conn)
        .await
        .map_err(Error::from)
}

/// Inserts one edge row after deriving its concrete runtime scope.
pub(super) async fn insert_edge_index(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: &EdgeWriteIntent,
) -> Result<bool> {
    let (tenant_id, contact_id) = runtime_ids_from_parts(
        store,
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        "edges",
    )?;
    let result = sqlx::query(
        r#"
        INSERT INTO moa.edge_index
            (uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id, contact_id,
             valid_from, properties)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (uid) DO NOTHING
        "#,
    )
    .bind(intent.uid)
    .bind(intent.label.as_str())
    .bind(intent.start_uid)
    .bind(intent.end_uid)
    .bind(intent.storage_partition_id.as_deref())
    .bind(intent.contact_id.as_deref())
    .bind(tenant_id)
    .bind(contact_id)
    .bind(intent.valid_from)
    .bind(&intent.properties)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Closes every still-active edge touching `uid` at `valid_to`.
///
/// Node supersession and invalidation call this so relationships die with the
/// node version they described instead of silently outliving it; history stays
/// readable through as-of walks.
pub(super) async fn close_incident_edges(
    conn: &mut PgConnection,
    uid: Uuid,
    valid_to: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.edge_index
        SET valid_to = $1
        WHERE (start_uid = $2 OR end_uid = $2)
          AND valid_to IS NULL
        "#,
    )
    .bind(valid_to)
    .bind(uid)
    .execute(conn)
    .await?;
    Ok(())
}

/// Closes every active edge touching any UID in one bounded invalidation batch.
pub(super) async fn close_incident_edges_batch(
    conn: &mut PgConnection,
    uids: &[Uuid],
    valid_to: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.edge_index
        SET valid_to = $1
        WHERE (start_uid = ANY($2::UUID[]) OR end_uid = ANY($2::UUID[]))
          AND valid_to IS NULL
        "#,
    )
    .bind(valid_to)
    .bind(uids)
    .execute(conn)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Inserts the internal edge that relates a replacement node to its predecessor.
pub(super) async fn insert_supersedes_edge_index(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    replacement_uid: Uuid,
    old_uid: Uuid,
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
    scope: &str,
    valid_from: DateTime<Utc>,
) -> Result<()> {
    validate_scope_shape(storage_partition_id, contact_id, scope)?;
    let (tenant_id, contact_uuid) = runtime_ids_from_parts(
        store,
        storage_partition_id,
        contact_id,
        "supersession edges",
    )?;
    sqlx::query(
        r#"
        INSERT INTO moa.edge_index
            (uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id, contact_id,
             valid_from, properties)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, '{}'::jsonb)
        ON CONFLICT (uid) DO NOTHING
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(EdgeLabel::Supersedes.as_str())
    .bind(replacement_uid)
    .bind(old_uid)
    .bind(storage_partition_id)
    .bind(contact_id)
    .bind(tenant_id)
    .bind(contact_uuid)
    .bind(valid_from)
    .execute(conn)
    .await?;
    Ok(())
}

/// Locks and validates both endpoints for one edge intent.
pub(super) async fn validate_edge_endpoints(
    conn: &mut PgConnection,
    intent: &EdgeWriteIntent,
) -> Result<()> {
    let start = fetch_stored_node(conn, intent.start_uid)
        .await?
        .ok_or(Error::NotFound(intent.start_uid))?;
    let end = fetch_stored_node(conn, intent.end_uid)
        .await?
        .ok_or(Error::NotFound(intent.end_uid))?;
    validate_loaded_edge_endpoints(intent, &start, &end)
}

/// Locks edge endpoints in deterministic UID order and validates a complete batch.
pub(super) async fn validate_edge_endpoints_batch(
    conn: &mut PgConnection,
    writes: &[ScopedEdgeWrite],
) -> Result<()> {
    let mut endpoint_uids = writes
        .iter()
        .flat_map(|write| [write.intent.start_uid, write.intent.end_uid])
        .collect::<Vec<_>>();
    endpoint_uids.sort_unstable();
    endpoint_uids.dedup();

    let rows = sqlx::query(
        r#"
        SELECT uid, label, storage_partition_id, user_id, tenant_id, data_subject_id,
               scope, pii_class, barrier, valid_from, valid_to, properties_summary
        FROM moa.node_index
        WHERE uid = ANY($1)
        ORDER BY uid
        FOR UPDATE
        "#,
    )
    .bind(&endpoint_uids)
    .fetch_all(conn)
    .await?;
    let mut endpoints = HashMap::with_capacity(rows.len());
    for row in rows {
        let uid = row.try_get::<Uuid, _>("uid")?;
        endpoints.insert(uid, stored_node_from_row(row)?);
    }

    for write in writes {
        let intent = &write.intent;
        let start = endpoints
            .get(&intent.start_uid)
            .ok_or(Error::NotFound(intent.start_uid))?;
        let end = endpoints
            .get(&intent.end_uid)
            .ok_or(Error::NotFound(intent.end_uid))?;
        validate_loaded_edge_endpoints(intent, start, end)?;
    }
    Ok(())
}

fn validate_loaded_edge_endpoints(
    intent: &EdgeWriteIntent,
    start: &StoredNode,
    end: &StoredNode,
) -> Result<()> {
    if start.valid_to.is_some() {
        return Err(Error::BiTemporal(format!(
            "{} is not active",
            intent.start_uid
        )));
    }
    if end.valid_to.is_some() {
        return Err(Error::BiTemporal(format!(
            "{} is not active",
            intent.end_uid
        )));
    }
    ensure_same_scope(intent, start, "edge endpoints must share the edge scope")?;
    ensure_same_scope(intent, end, "edge endpoints must share the edge scope")
}
