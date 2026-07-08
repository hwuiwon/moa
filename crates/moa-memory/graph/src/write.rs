//! Atomic graph write protocol for relational rows, vectors, and changelog records.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use moa_memory_vector::{VECTOR_DIMENSION, VectorItem, VectorStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{
    GraphError, PostgresGraphStore, Result,
    changelog::{ChangelogRecord, write_and_bump},
    edge::{EdgeLabel, EdgeWriteIntent},
    node::{
        ExistingSupersessionIntent, NodeEmbeddingIntent, NodeExpiryIntent, NodeLabel,
        NodePropertyUpdateIntent, NodeReinforcementIntent, NodeWriteIntent, PiiClass,
    },
};

/// Creates a graph node, sidecar row, optional vector, and changelog row atomically.
pub async fn create_node(store: &PostgresGraphStore, intent: NodeWriteIntent) -> Result<Uuid> {
    let mut conn = store.begin_required().await?;
    let uid = create_node_in_conn(store, conn.as_mut(), intent).await?;
    conn.commit().await?;
    sync_vector_post_commit(store, "create_node").await;
    Ok(uid)
}

/// Creates a graph node, sidecar row, optional vector, and changelog row in a caller-owned tx.
pub async fn create_node_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: NodeWriteIntent,
) -> Result<Uuid> {
    validate_node_scope(&intent)?;
    let vector_item = vector_item_from_intent(&intent)?;

    insert_node_index(store, &mut *conn, &intent).await?;
    if let Some(item) = vector_item.as_ref() {
        ensure_storage_partition_embedder_state(
            &mut *conn,
            intent.storage_partition_id.as_deref(),
            &item.embedding_model,
            item.embedding_model_version,
        )
        .await?;
        let vector = require_vector_store(store)?;
        vector
            .upsert_in_tx(&mut *conn, std::slice::from_ref(item))
            .await?;
    }
    write_and_bump(&mut *conn, create_changelog(&intent, None)).await?;

    Ok(intent.uid)
}

/// Creates several graph nodes, sidecar rows, optional vectors, and changelog rows atomically.
///
/// All nodes are written inside one transaction: the `node_index` rows are
/// inserted with a single `UNNEST` multi-row statement (JSON travels as `TEXT[]`
/// cast to `JSONB`, mirroring the tenant-knowledge batch inserts), every
/// embedding-bearing node's vector is upserted in one call, and each node still
/// writes its own `graph_changelog` outbox row so the per-node create audit and
/// the storage-partition version bump match a loop of [`create_node`]. Returns
/// the created uids in input order.
pub async fn bulk_create_nodes(
    store: &PostgresGraphStore,
    intents: Vec<NodeWriteIntent>,
) -> Result<Vec<Uuid>> {
    if intents.is_empty() {
        return Ok(Vec::new());
    }
    for intent in &intents {
        validate_node_scope(intent)?;
    }

    let count = intents.len();
    let mut uids = Vec::with_capacity(count);
    let mut labels = Vec::with_capacity(count);
    let mut storage_partition_ids: Vec<Option<String>> = Vec::with_capacity(count);
    let mut user_ids: Vec<Option<String>> = Vec::with_capacity(count);
    let mut tenant_ids = Vec::with_capacity(count);
    let mut contact_ids: Vec<Option<Uuid>> = Vec::with_capacity(count);
    let mut names = Vec::with_capacity(count);
    let mut pii_classes = Vec::with_capacity(count);
    let mut confidences: Vec<Option<f64>> = Vec::with_capacity(count);
    let mut reference_counts = Vec::with_capacity(count);
    let mut valid_froms = Vec::with_capacity(count);
    let mut properties = Vec::with_capacity(count);
    let mut vector_items = Vec::new();
    let mut vector_state_seeds = Vec::new();
    for intent in &intents {
        let (tenant_id, contact_id) = runtime_ids_for_node(store, intent)?;
        if let Some(item) = vector_item_from_intent(intent)? {
            vector_state_seeds.push((
                intent.storage_partition_id.clone(),
                item.embedding_model.clone(),
                item.embedding_model_version,
            ));
            vector_items.push(item);
        }
        uids.push(intent.uid);
        labels.push(intent.label.as_str().to_string());
        storage_partition_ids.push(intent.storage_partition_id.clone());
        user_ids.push(intent.contact_id.clone());
        tenant_ids.push(tenant_id);
        contact_ids.push(contact_id);
        names.push(intent.name.clone());
        pii_classes.push(intent.pii_class.as_str().to_string());
        confidences.push(intent.confidence);
        reference_counts.push(reference_count_from_properties(&intent.properties));
        valid_froms.push(intent.valid_from);
        properties.push(serde_json::to_string(&intent.properties)?);
    }

    let mut conn = store.begin_required().await?;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class,
             confidence, reference_count, valid_from, properties_summary)
        SELECT n.uid, n.label, n.storage_partition_id, n.user_id, n.tenant_id, n.contact_id,
               n.name, n.pii_class, n.confidence, n.reference_count, n.valid_from,
               n.properties::JSONB
        FROM UNNEST(
            $1::UUID[], $2::TEXT[], $3::TEXT[], $4::TEXT[], $5::UUID[], $6::UUID[], $7::TEXT[],
            $8::TEXT[], $9::DOUBLE PRECISION[], $10::BIGINT[], $11::TIMESTAMPTZ[], $12::TEXT[]
        ) AS n(uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class,
               confidence, reference_count, valid_from, properties)
        "#,
    )
    .bind(&uids)
    .bind(&labels)
    .bind(&storage_partition_ids)
    .bind(&user_ids)
    .bind(&tenant_ids)
    .bind(&contact_ids)
    .bind(&names)
    .bind(&pii_classes)
    .bind(&confidences)
    .bind(&reference_counts)
    .bind(&valid_froms)
    .bind(&properties)
    .execute(conn.as_mut())
    .await?;

    if !vector_items.is_empty() {
        for (storage_partition_id, embedding_model, embedding_model_version) in &vector_state_seeds
        {
            ensure_storage_partition_embedder_state(
                conn.as_mut(),
                storage_partition_id.as_deref(),
                embedding_model,
                *embedding_model_version,
            )
            .await?;
        }
        let vector = require_vector_store(store)?;
        vector.upsert_in_tx(conn.as_mut(), &vector_items).await?;
    }

    for intent in &intents {
        write_and_bump(conn.as_mut(), create_changelog(intent, None)).await?;
    }

    conn.commit().await?;
    sync_vector_post_commit(store, "bulk_create_nodes").await;
    Ok(uids)
}

/// Supersedes one active graph node with a replacement node atomically.
pub async fn supersede_node(
    store: &PostgresGraphStore,
    old_uid: Uuid,
    new: NodeWriteIntent,
) -> Result<Uuid> {
    let mut conn = store.begin_required().await?;
    let uid = supersede_node_in_conn(store, conn.as_mut(), old_uid, new).await?;
    conn.commit().await?;
    sync_vector_post_commit(store, "supersede_node").await;
    Ok(uid)
}

/// Supersedes one active graph node with a replacement node in a caller-owned tx.
///
/// Callers that batch several graph mutations into a single transaction use this
/// primitive so the relational rows, sidecar vector, and changelog records commit
/// together; unlike [`supersede_node`], it neither commits nor drains the external
/// vector-sync outbox, leaving both to the caller.
pub async fn supersede_node_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    old_uid: Uuid,
    mut new: NodeWriteIntent,
) -> Result<Uuid> {
    validate_node_scope(&new)?;
    let (current_uid, old) = fetch_current_supersession_target(&mut *conn, old_uid).await?;
    ensure_same_scope(&new, &old, "supersession nodes must share the same scope")?;
    if new.valid_from <= old.valid_from {
        new.valid_from = old.valid_from + Duration::microseconds(1);
    }
    let vector_item = vector_item_from_intent(&new)?;

    let now = Utc::now();
    close_node_index(
        &mut *conn,
        current_uid,
        new.valid_from,
        now,
        actor_uuid(&new.actor_id),
        "superseded",
    )
    .await?;
    close_incident_edges(&mut *conn, current_uid, new.valid_from).await?;
    insert_node_index(store, &mut *conn, &new).await?;
    insert_supersedes_edge_index(
        store,
        &mut *conn,
        new.uid,
        current_uid,
        new.storage_partition_id.as_deref(),
        new.contact_id.as_deref(),
        &new.scope,
        new.valid_from,
    )
    .await?;

    if let Some(vector) = store.vector() {
        vector.delete_in_tx(&mut *conn, &[current_uid]).await?;
        if let Some(item) = vector_item.as_ref() {
            ensure_storage_partition_embedder_state(
                &mut *conn,
                new.storage_partition_id.as_deref(),
                &item.embedding_model,
                item.embedding_model_version,
            )
            .await?;
            vector
                .upsert_in_tx(&mut *conn, std::slice::from_ref(item))
                .await?;
        }
    } else if vector_item.is_some() {
        return Err(GraphError::Conflict(
            "embedding provided but no vector store is configured".to_string(),
        ));
    }

    let old_change = write_and_bump(
        &mut *conn,
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id.clone(),
            contact_id: old.contact_id.clone(),
            scope: old.scope.clone(),
            actor_id: Some(new.actor_id.clone()),
            actor_kind: new.actor_kind.clone(),
            op: "supersede".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: current_uid,
            payload: json!({
                "before": old.properties_summary,
                "valid_to": new.valid_from.to_rfc3339(),
                "replacement_uid": new.uid,
            }),
            redaction_marker: None,
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;
    write_and_bump(&mut *conn, create_changelog(&new, Some(old_change))).await?;

    Ok(new.uid)
}

/// Soft-invalidates one graph node and removes its vector projection atomically.
pub async fn invalidate_node(store: &PostgresGraphStore, uid: Uuid, reason: &str) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), uid)
        .await?
        .ok_or(GraphError::NotFound(uid))?;
    if old.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{uid} is already invalidated"
        )));
    }

    let now = Utc::now();
    let (actor_id, actor_kind) = mutation_actor(store);
    close_node_index(
        conn.as_mut(),
        uid,
        now,
        now,
        actor_id.as_deref().and_then(actor_uuid),
        reason,
    )
    .await?;
    close_incident_edges(conn.as_mut(), uid, now).await?;
    if let Some(vector) = store.vector() {
        vector.delete_in_tx(conn.as_mut(), &[uid]).await?;
    }
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id,
            contact_id: old.contact_id,
            scope: old.scope,
            actor_id,
            actor_kind,
            op: "invalidate".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: uid,
            payload: json!({
                "before": old.properties_summary,
                "reason": reason,
                "valid_to": now.to_rfc3339(),
            }),
            redaction_marker: None,
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    sync_vector_post_commit(store, "invalidate_node").await;
    Ok(())
}

/// Closes one active graph node into an already-existing replacement node atomically.
pub async fn close_existing_node_with_supersession(
    store: &PostgresGraphStore,
    intent: ExistingSupersessionIntent,
) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.old_uid)
        .await?
        .ok_or(GraphError::NotFound(intent.old_uid))?;
    let replacement = fetch_stored_node(conn.as_mut(), intent.replacement_uid)
        .await?
        .ok_or(GraphError::NotFound(intent.replacement_uid))?;
    if old.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is already invalidated",
            intent.old_uid
        )));
    }
    if replacement.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is not an active replacement",
            intent.replacement_uid
        )));
    }
    ensure_same_scope(
        &old,
        &replacement,
        "supersession nodes must share the same scope",
    )?;

    close_node_index(
        conn.as_mut(),
        intent.old_uid,
        intent.valid_to,
        intent.invalidated_at,
        actor_uuid(&intent.actor_id),
        &intent.reason,
    )
    .await?;
    close_incident_edges(conn.as_mut(), intent.old_uid, intent.valid_to).await?;
    insert_supersedes_edge_index(
        store,
        conn.as_mut(),
        intent.replacement_uid,
        intent.old_uid,
        old.storage_partition_id.as_deref(),
        old.contact_id.as_deref(),
        &old.scope,
        intent.valid_to,
    )
    .await?;
    if let Some(vector) = store.vector() {
        vector
            .delete_in_tx(conn.as_mut(), &[intent.old_uid])
            .await?;
    }
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id,
            contact_id: old.contact_id,
            scope: old.scope,
            actor_id: Some(intent.actor_id),
            actor_kind: intent.actor_kind,
            op: "supersede".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: intent.old_uid,
            payload: json!({
                "before": old.properties_summary,
                "valid_to": intent.valid_to.to_rfc3339(),
                "replacement_uid": intent.replacement_uid,
                "reason": intent.reason,
            }),
            redaction_marker: None,
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    sync_vector_post_commit(store, "close_existing_node_with_supersession").await;
    Ok(())
}

/// Closes one active graph node without a replacement, at caller-provided instants.
///
/// Bitemporal close only: the node row, its incident edges, and its vector rows
/// are closed or removed, and a changelog record is written — history and as-of
/// reads keep working. Returns `false` without writing when the node is already
/// closed, so scheduled passes rerun idempotently at the same `now`.
pub async fn expire_node(store: &PostgresGraphStore, intent: NodeExpiryIntent) -> Result<bool> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(GraphError::NotFound(intent.uid))?;
    if old.valid_to.is_some() {
        return Ok(false);
    }

    close_node_index(
        conn.as_mut(),
        intent.uid,
        intent.valid_to,
        intent.invalidated_at,
        actor_uuid(&intent.actor_id),
        &intent.reason,
    )
    .await?;
    close_incident_edges(conn.as_mut(), intent.uid, intent.valid_to).await?;
    if let Some(vector) = store.vector() {
        vector.delete_in_tx(conn.as_mut(), &[intent.uid]).await?;
    }
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id,
            contact_id: old.contact_id,
            scope: old.scope,
            actor_id: Some(intent.actor_id),
            actor_kind: intent.actor_kind,
            op: "invalidate".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: intent.uid,
            payload: json!({
                "before": old.properties_summary,
                "valid_to": intent.valid_to.to_rfc3339(),
                "reason": intent.reason,
            }),
            redaction_marker: None,
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    sync_vector_post_commit(store, "expire_node").await;
    Ok(true)
}

/// Updates one active graph node's mutable properties atomically.
pub async fn update_node_properties(
    store: &PostgresGraphStore,
    intent: NodePropertyUpdateIntent,
) -> Result<()> {
    if !intent.properties.is_object() {
        return Err(GraphError::Conflict(
            "node properties must be a JSON object".to_string(),
        ));
    }

    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(GraphError::NotFound(intent.uid))?;
    if old.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is not active",
            intent.uid
        )));
    }
    let result = sqlx::query(
        r#"
        UPDATE moa.node_index
        SET properties_summary = $1
        WHERE uid = $2
          AND valid_to IS NULL
        "#,
    )
    .bind(&intent.properties)
    .bind(intent.uid)
    .execute(conn.as_mut())
    .await?;
    if result.rows_affected() == 0 {
        return Err(GraphError::BiTemporal(format!(
            "{} is not active",
            intent.uid
        )));
    }
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id,
            contact_id: old.contact_id,
            scope: old.scope,
            actor_id: Some(intent.actor_id),
            actor_kind: intent.actor_kind,
            op: "update".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: intent.uid,
            payload: json!({
                "before": old.properties_summary,
                "after": intent.properties,
            }),
            redaction_marker: None,
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    Ok(())
}

/// Reinforces one active node that ingestion re-observed, in its own transaction.
///
/// Bumps `confidence` one `step` toward `cap` (never lowering an already higher
/// confidence), drops the `base_confidence` decay anchor so the next
/// consolidation decay re-anchors from the boosted value, and touches
/// `last_accessed_at` so idle decay and last-access recency ranking treat the
/// fact as live. Like consolidation decay, this adjusts derived ranking
/// metadata only, so no changelog row is written. Returns `true` when an
/// active row was updated.
pub async fn reinforce_node(
    store: &PostgresGraphStore,
    intent: NodeReinforcementIntent,
) -> Result<bool> {
    let mut conn = store.begin_required().await?;
    let reinforced = reinforce_node_in_conn(conn.as_mut(), intent).await?;
    conn.commit().await?;
    Ok(reinforced)
}

/// Reinforces one active node inside a caller-owned scoped transaction.
///
/// See [`reinforce_node`] for semantics.
pub async fn reinforce_node_in_conn(
    conn: &mut PgConnection,
    intent: NodeReinforcementIntent,
) -> Result<bool> {
    let result = sqlx::query(
        r#"
        UPDATE moa.node_index
        SET confidence = GREATEST(
                COALESCE(confidence, 0.5),
                LEAST(COALESCE(confidence, 0.5) + $2, $3)
            ),
            properties_summary = properties_summary - 'base_confidence',
            last_accessed_at = now()
        WHERE uid = $1
          AND valid_to IS NULL
        "#,
    )
    .bind(intent.uid)
    .bind(intent.step)
    .bind(intent.cap)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Attaches a vector embedding to one active graph node atomically.
pub async fn upsert_node_embedding(
    store: &PostgresGraphStore,
    intent: NodeEmbeddingIntent,
) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let node = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(GraphError::NotFound(intent.uid))?;
    if node.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is not active",
            intent.uid
        )));
    }
    let vector = require_vector_store(store)?;
    ensure_storage_partition_embedder_state(
        conn.as_mut(),
        node.storage_partition_id.as_deref(),
        &intent.embedding_model,
        intent.embedding_model_version,
    )
    .await?;
    vector
        .upsert_in_tx(
            conn.as_mut(),
            &[VectorItem {
                uid: intent.uid,
                user_id: node.contact_id.clone(),
                label: node.label.as_str().to_string(),
                pii_class: node.pii_class.as_str().to_string(),
                embedding: intent.embedding,
                embedding_model: intent.embedding_model.clone(),
                embedding_model_version: intent.embedding_model_version,
                search_text: None,
                valid_to: None,
            }],
        )
        .await?;
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: node.storage_partition_id,
            contact_id: node.contact_id,
            scope: node.scope,
            actor_id: Some(intent.actor_id),
            actor_kind: intent.actor_kind,
            op: "update".to_string(),
            target_kind: "node".to_string(),
            target_label: node.label.as_str().to_string(),
            target_uid: intent.uid,
            payload: json!({
                "embedding_model": intent.embedding_model,
                "embedding_model_version": intent.embedding_model_version,
            }),
            redaction_marker: None,
            pii_class: node.pii_class.as_str().to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    sync_vector_post_commit(store, "upsert_node_embedding").await;
    Ok(())
}

/// Hard-purges one graph node while preserving a redacted audit changelog row.
pub async fn hard_purge(
    store: &PostgresGraphStore,
    uid: Uuid,
    redaction_marker: &str,
) -> Result<()> {
    hard_purge_with_audit(store, uid, redaction_marker, None).await
}

/// Hard-purges one graph node with explicit audit metadata on the erase changelog row.
pub async fn hard_purge_with_audit(
    store: &PostgresGraphStore,
    uid: Uuid,
    redaction_marker: &str,
    audit_metadata: Option<Value>,
) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), uid)
        .await?
        .ok_or(GraphError::NotFound(uid))?;
    let (actor_id, actor_kind) = mutation_actor(store);
    let properties_hash = hash_properties(old.properties_summary.as_ref())?;

    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: old.storage_partition_id.clone(),
            contact_id: old.contact_id.clone(),
            scope: old.scope.clone(),
            actor_id,
            actor_kind,
            op: "erase".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: uid,
            payload: json!({
                "redaction_marker": redaction_marker,
                "label": old.label.as_str(),
                "scope": old.scope,
                "properties_hash": properties_hash,
            }),
            redaction_marker: Some(redaction_marker.to_string()),
            pii_class: old.pii_class.as_str().to_string(),
            audit_metadata,
            cause_change_id: None,
        },
    )
    .await?;
    sqlx::query("DELETE FROM moa.edge_index WHERE start_uid = $1 OR end_uid = $1")
        .bind(uid)
        .execute(conn.as_mut())
        .await?;
    if let Some(vector) = store.vector() {
        vector.delete_in_tx(conn.as_mut(), &[uid]).await?;
    }
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(conn.as_mut())
        .await?;

    conn.commit().await?;
    sync_vector_post_commit(store, "hard_purge").await;
    Ok(())
}

/// Creates a graph edge and changelog row atomically.
pub async fn create_edge(store: &PostgresGraphStore, intent: EdgeWriteIntent) -> Result<Uuid> {
    let mut conn = store.begin_required().await?;
    let uid = create_edge_in_conn(store, conn.as_mut(), intent).await?;
    conn.commit().await?;
    Ok(uid)
}

/// Creates a graph edge and changelog row in a caller-owned transaction.
///
/// Batching callers use this primitive to write several edges alongside their
/// node mutations inside one transaction; unlike [`create_edge`], it does not
/// commit. Edges never write sidecar vectors, so there is no vector-sync work.
pub async fn create_edge_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: EdgeWriteIntent,
) -> Result<Uuid> {
    validate_edge_scope(&intent)?;
    if edge_exists(&mut *conn, intent.uid).await? {
        return Ok(intent.uid);
    }
    validate_edge_endpoints(&mut *conn, &intent).await?;
    if !insert_edge_index(store, &mut *conn, &intent).await? {
        return Ok(intent.uid);
    }
    write_and_bump(
        &mut *conn,
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
        },
    )
    .await?;

    Ok(intent.uid)
}

async fn edge_exists(conn: &mut PgConnection, uid: Uuid) -> Result<bool> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM moa.edge_index WHERE uid = $1)")
        .bind(uid)
        .fetch_one(conn)
        .await
        .map_err(GraphError::from)
}

async fn insert_edge_index(
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
async fn close_incident_edges(
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

/// Soft-invalidates one graph edge by closing its validity window.
pub async fn invalidate_edge(store: &PostgresGraphStore, uid: Uuid, reason: &str) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let row = sqlx::query(
        r#"
        SELECT label, storage_partition_id, user_id, scope, valid_to
        FROM moa.edge_index
        WHERE uid = $1
        FOR UPDATE
        "#,
    )
    .bind(uid)
    .fetch_optional(conn.as_mut())
    .await?
    .ok_or(GraphError::NotFound(uid))?;
    let valid_to: Option<DateTime<Utc>> = row.try_get("valid_to")?;
    if valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{uid} is already invalidated"
        )));
    }

    let now = Utc::now();
    sqlx::query("UPDATE moa.edge_index SET valid_to = $1 WHERE uid = $2 AND valid_to IS NULL")
        .bind(now)
        .bind(uid)
        .execute(conn.as_mut())
        .await?;
    let (actor_id, actor_kind) = mutation_actor(store);
    let label: String = row.try_get("label")?;
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            storage_partition_id: row.try_get("storage_partition_id")?,
            contact_id: row.try_get("user_id")?,
            scope: row.try_get("scope")?,
            actor_id,
            actor_kind,
            op: "invalidate".to_string(),
            target_kind: "edge".to_string(),
            target_label: label,
            target_uid: uid,
            payload: json!({
                "reason": reason,
                "valid_to": now.to_rfc3339(),
            }),
            redaction_marker: None,
            pii_class: "none".to_string(),
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;

    conn.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn insert_supersedes_edge_index(
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

async fn validate_edge_endpoints(conn: &mut PgConnection, intent: &EdgeWriteIntent) -> Result<()> {
    let start = fetch_stored_node(conn, intent.start_uid)
        .await?
        .ok_or(GraphError::NotFound(intent.start_uid))?;
    let end = fetch_stored_node(conn, intent.end_uid)
        .await?
        .ok_or(GraphError::NotFound(intent.end_uid))?;
    if start.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is not active",
            intent.start_uid
        )));
    }
    if end.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{} is not active",
            intent.end_uid
        )));
    }
    ensure_same_scope(intent, &start, "edge endpoints must share the edge scope")?;
    ensure_same_scope(intent, &end, "edge endpoints must share the edge scope")?;
    ensure_same_scope(&start, &end, "supersession nodes must share the same scope")
}

fn validate_node_scope(intent: &NodeWriteIntent) -> Result<()> {
    validate_scope_shape(
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        &intent.scope,
    )?;
    if !intent.properties.is_object() {
        return Err(GraphError::Conflict(
            "node properties must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

fn validate_edge_scope(intent: &EdgeWriteIntent) -> Result<()> {
    validate_scope_shape(
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        &intent.scope,
    )?;
    if !intent.properties.is_object() {
        return Err(GraphError::Conflict(
            "edge properties must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

/// Returns the expected scope tier for a storage-partition/contact pair.
///
/// Returns `None` for the invalid combination of a contact without a storage
/// partition; callers map that to their own error.
pub(crate) fn expected_scope_tier(
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
) -> Option<&'static str> {
    match (storage_partition_id, contact_id) {
        (None, None) => Some("global"),
        (Some(_), None) => Some("tenant"),
        (Some(_), Some(_)) => Some("contact"),
        (None, Some(_)) => None,
    }
}

fn validate_scope_shape(
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
    scope: &str,
) -> Result<()> {
    let expected = expected_scope_tier(storage_partition_id, contact_id).ok_or_else(|| {
        GraphError::Conflict("contact scope requires storage partition".to_string())
    })?;
    if scope == expected {
        Ok(())
    } else {
        Err(GraphError::Conflict(format!(
            "scope `{scope}` does not match computed scope `{expected}`"
        )))
    }
}

/// Provides the `(storage_partition_id, contact_id, scope)` tuple used for
/// scope-equality checks across nodes and edges.
trait ScopeTriple {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str);
}

impl ScopeTriple for StoredNode {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

impl ScopeTriple for NodeWriteIntent {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

impl ScopeTriple for EdgeWriteIntent {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

/// Returns a [`GraphError::Conflict`] with `message` when the two scope triples differ.
fn ensure_same_scope(a: &impl ScopeTriple, b: &impl ScopeTriple, message: &str) -> Result<()> {
    if a.scope_triple() == b.scope_triple() {
        Ok(())
    } else {
        Err(GraphError::Conflict(message.to_string()))
    }
}

async fn insert_node_index(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: &NodeWriteIntent,
) -> Result<()> {
    let (tenant_id, contact_id) = runtime_ids_for_node(store, intent)?;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class, confidence,
             reference_count, valid_from, properties_summary)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
    )
    .bind(intent.uid)
    .bind(intent.label.as_str())
    .bind(intent.storage_partition_id.as_deref())
    .bind(intent.contact_id.as_deref())
    .bind(tenant_id)
    .bind(contact_id)
    .bind(&intent.name)
    .bind(intent.pii_class.as_str())
    .bind(intent.confidence)
    .bind(reference_count_from_properties(&intent.properties))
    .bind(intent.valid_from)
    .bind(&intent.properties)
    .execute(conn)
    .await?;
    Ok(())
}

fn runtime_ids_for_node(
    store: &PostgresGraphStore,
    intent: &NodeWriteIntent,
) -> Result<(Uuid, Option<Uuid>)> {
    if let Some(scope) = store.scope() {
        return Ok((scope.tenant_id().0, scope.contact_id().map(|id| id.0)));
    }

    let Some(storage_partition_id) = intent.storage_partition_id.as_deref() else {
        return Err(GraphError::Conflict(
            "tenant-owned graph nodes require tenant scope".to_string(),
        ));
    };
    let tenant_id = parse_uuid(storage_partition_id, "storage partition", "tenant_id")?;
    Ok((tenant_id, None))
}

fn runtime_ids_from_parts(
    store: &PostgresGraphStore,
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
    target: &str,
) -> Result<(Uuid, Option<Uuid>)> {
    if let Some(scope) = store.scope() {
        return Ok((scope.tenant_id().0, scope.contact_id().map(|id| id.0)));
    }

    let Some(storage_partition_id) = storage_partition_id else {
        return Err(GraphError::Conflict(format!(
            "tenant-owned graph {target} require tenant scope"
        )));
    };
    let tenant_id = parse_uuid(storage_partition_id, "storage partition", "tenant_id")?;
    let contact_id = contact_id
        .map(|value| parse_uuid(value, "contact", "contact_id"))
        .transpose()?;
    Ok((tenant_id, contact_id))
}

fn parse_uuid(value: &str, value_kind: &str, column: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        GraphError::Conflict(format!(
            "{value_kind} `{value}` cannot be used as {column}: {error}"
        ))
    })
}

fn reference_count_from_properties(properties: &Value) -> i64 {
    properties
        .get("reference_count")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(0)
}

async fn close_node_index(
    conn: &mut PgConnection,
    uid: Uuid,
    valid_to: DateTime<Utc>,
    invalidated_at: DateTime<Utc>,
    invalidated_by: Option<Uuid>,
    reason: &str,
) -> Result<()> {
    let result = sqlx::query(
        r#"
        UPDATE moa.node_index
        SET valid_to = $1,
            invalidated_at = $2,
            invalidated_by = $3,
            invalidated_reason = $4
        WHERE uid = $5
          AND valid_to IS NULL
        "#,
    )
    .bind(valid_to)
    .bind(invalidated_at)
    .bind(invalidated_by)
    .bind(reason)
    .bind(uid)
    .execute(conn)
    .await?;
    if result.rows_affected() == 0 {
        Err(GraphError::BiTemporal(format!(
            "{uid} was already invalidated"
        )))
    } else {
        Ok(())
    }
}

fn vector_item_from_intent(intent: &NodeWriteIntent) -> Result<Option<VectorItem>> {
    let Some(embedding) = intent.embedding.clone() else {
        return Ok(None);
    };
    let Some(embedding_model) = intent.embedding_model.clone() else {
        return Err(GraphError::MissingEmbeddingMetadata);
    };
    let Some(embedding_model_version) = intent.embedding_model_version else {
        return Err(GraphError::MissingEmbeddingMetadata);
    };
    Ok(Some(VectorItem {
        uid: intent.uid,
        user_id: intent.contact_id.clone(),
        label: intent.label.as_str().to_string(),
        pii_class: intent.pii_class.as_str().to_string(),
        embedding,
        embedding_model,
        embedding_model_version,
        search_text: intent.embedding_text.clone(),
        valid_to: None,
    }))
}

async fn ensure_storage_partition_embedder_state(
    conn: &mut PgConnection,
    storage_partition_id: Option<&str>,
    embedding_model: &str,
    embedding_model_version: i32,
) -> Result<()> {
    let storage_partition_id = storage_partition_id.ok_or_else(|| {
        GraphError::Conflict("embedding writes require storage partition state".to_string())
    })?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO NOTHING
        "#,
    )
    .bind(storage_partition_id)
    .bind(embedding_model)
    .bind(embedding_model_version)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn)
    .await?;
    Ok(())
}

fn require_vector_store(store: &PostgresGraphStore) -> Result<&dyn VectorStore> {
    store.vector().ok_or_else(|| {
        GraphError::Conflict("embedding provided but no vector store is configured".to_string())
    })
}

async fn sync_vector_post_commit(store: &PostgresGraphStore, operation: &'static str) {
    let Some(hook) = store.vector_post_commit_sync() else {
        return;
    };
    if let Err(error) = hook.sync_post_commit().await {
        tracing::warn!(
            error = %error,
            operation,
            "post-commit vector sync failed; queued rows remain pending"
        );
    }
}

fn create_changelog(intent: &NodeWriteIntent, cause_change_id: Option<i64>) -> ChangelogRecord {
    ChangelogRecord {
        storage_partition_id: intent.storage_partition_id.clone(),
        contact_id: intent.contact_id.clone(),
        scope: intent.scope.clone(),
        actor_id: Some(intent.actor_id.clone()),
        actor_kind: intent.actor_kind.clone(),
        op: "create".to_string(),
        target_kind: "node".to_string(),
        target_label: intent.label.as_str().to_string(),
        target_uid: intent.uid,
        payload: json!({ "after": intent.properties }),
        redaction_marker: None,
        pii_class: intent.pii_class.as_str().to_string(),
        audit_metadata: None,
        cause_change_id,
    }
}

fn mutation_actor(store: &PostgresGraphStore) -> (Option<String>, String) {
    store
        .scope()
        .and_then(|scope| scope.contact_id())
        .map(|contact_id| (Some(contact_id.to_string()), "contact".to_string()))
        .unwrap_or((None, "system".to_string()))
}

fn actor_uuid(actor_id: &str) -> Option<Uuid> {
    Uuid::parse_str(actor_id).ok()
}

fn hash_properties(properties: Option<&Value>) -> Result<String> {
    let bytes = serde_json::to_vec(properties.unwrap_or(&Value::Null))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

async fn fetch_stored_node(conn: &mut PgConnection, uid: Uuid) -> Result<Option<StoredNode>> {
    let row = sqlx::query(
        r#"
        SELECT label, storage_partition_id, user_id, scope, pii_class, valid_from,
               valid_to, properties_summary
        FROM moa.node_index
        WHERE uid = $1
        FOR UPDATE
        "#,
    )
    .bind(uid)
    .fetch_optional(conn)
    .await?;
    row.map(stored_node_from_row).transpose()
}

async fn fetch_current_supersession_target(
    conn: &mut PgConnection,
    initial_uid: Uuid,
) -> Result<(Uuid, StoredNode)> {
    let mut uid = initial_uid;
    let mut seen = HashSet::new();

    loop {
        if !seen.insert(uid) {
            return Err(GraphError::BiTemporal(format!(
                "supersession cycle detected while resolving {initial_uid}"
            )));
        }

        let stored = fetch_stored_node(conn, uid)
            .await?
            .ok_or(GraphError::NotFound(uid))?;
        if stored.valid_to.is_none() {
            return Ok((uid, stored));
        }

        let replacement_uid = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT (payload->>'replacement_uid')::uuid
              FROM moa.graph_changelog
             WHERE target_uid = $1
               AND op = 'supersede'
             ORDER BY change_id DESC
             LIMIT 1
            "#,
        )
        .bind(uid)
        .fetch_optional(&mut *conn)
        .await?;

        match replacement_uid {
            Some(next_uid) => uid = next_uid,
            None => {
                return Err(GraphError::BiTemporal(format!(
                    "{uid} is invalidated and has no supersession replacement"
                )));
            }
        }
    }
}

fn stored_node_from_row(row: sqlx::postgres::PgRow) -> Result<StoredNode> {
    let label_text: String = row.try_get("label")?;
    let pii_class_text: String = row.try_get("pii_class")?;
    Ok(StoredNode {
        label: label_text.parse()?,
        storage_partition_id: row.try_get("storage_partition_id")?,
        contact_id: row.try_get("user_id")?,
        scope: row.try_get("scope")?,
        pii_class: pii_class_text.parse()?,
        valid_from: row.try_get("valid_from")?,
        valid_to: row.try_get("valid_to")?,
        properties_summary: row.try_get("properties_summary")?,
    })
}

#[derive(Debug, Clone)]
struct StoredNode {
    label: NodeLabel,
    storage_partition_id: Option<String>,
    contact_id: Option<String>,
    scope: String,
    pii_class: PiiClass,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    properties_summary: Option<Value>,
}
