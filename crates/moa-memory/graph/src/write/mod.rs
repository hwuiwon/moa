//! Atomic graph write protocol for relational rows, vectors, and changelog records.

mod edge;
mod node;
mod scope;
mod sealed;

use std::collections::{HashMap, HashSet};

use chrono::{Duration, Utc};
use moa_core::types::memory::InformationBarrierId;
use moa_memory_vector::VectorItem;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    Error, MAX_BULK_INVALIDATE_NODES, PostgresGraphStore, Result,
    changelog::{ChangelogRecord, write_and_bump, write_batch_and_bump},
    edge::EdgeWriteIntent,
    node::{
        ExistingSupersessionIntent, NodeContentUpdateIntent, NodeEmbeddingIntent, NodeExpiryIntent,
        NodeReinforcementIntent, NodeWriteIntent,
    },
};

use self::{
    edge::{
        ScopedEdgeWrite, close_incident_edges, close_incident_edges_batch, edge_changelog,
        edge_exists, insert_edge_index, insert_edge_index_batch, insert_supersedes_edge_index,
        validate_edge_endpoints, validate_edge_endpoints_batch,
    },
    node::{
        actor_uuid, close_node_index, create_changelog, ensure_storage_partition_embedder_state,
        fetch_current_supersession_target, fetch_stored_node, fetch_stored_nodes, hash_properties,
        insert_node_index, mutation_actor, reference_count_from_properties, require_vector_store,
        vector_item_from_intent, write_created_node,
    },
    scope::{
        ensure_same_scope, install_write_barriers, runtime_ids_for_node, runtime_ids_from_parts,
        validate_edge_scope, validate_node_scope,
    },
    sealed::{prepare_node_fields, prepare_node_fields_batch},
};

pub(crate) use scope::expected_scope_tier;
pub(crate) use sealed::{SEALED_CONTENT_VERSION, SealedNodeContent};

/// Maximum attempts for a write transaction that aborts on a Postgres
/// serialization deadlock (SQLSTATE `40P01`).
const MAX_DEADLOCK_RETRIES: u32 = 5;

/// Returns whether `error` is a Postgres deadlock (`40P01`).
///
/// Concurrent ingestion transactions that touch the same graph rows — shared
/// entity nodes reused across documents, plus the per-partition changelog bump
/// on `moa.storage_partition_state` — can be chosen as the deadlock victim.
/// Deterministic lock ordering removes the common cycle; this predicate lets the
/// remaining, rare cases be retried instead of aborting the caller.
fn is_deadlock(error: &Error) -> bool {
    matches!(
        error,
        Error::Sidecar(sqlx_error)
            if sqlx_error
                .as_database_error()
                .and_then(|db| db.code())
                .as_deref()
                == Some("40P01")
    )
}

/// Sleeps a short, jittered backoff before retrying a deadlocked transaction.
///
/// Exponential in the attempt with per-call jitter so two writers that collided
/// do not re-collide on an identical retry schedule.
async fn deadlock_backoff(attempt: u32) {
    tokio::time::sleep(full_jitter_delay(4, 128, attempt, rand::random())).await;
}

/// Computes full-jitter exponential backoff from a deterministic random sample.
///
/// The returned delay is uniformly selected from zero through the capped
/// exponential ceiling. Passing the sample explicitly keeps the policy exactly
/// unit-testable while callers use operating-system randomness.
pub(crate) fn full_jitter_delay(
    base_ms: u64,
    cap_ms: u64,
    attempt: u32,
    sample: u64,
) -> std::time::Duration {
    let multiplier = 1_u64.checked_shl(attempt.min(63)).unwrap_or(u64::MAX);
    let ceiling = base_ms.saturating_mul(multiplier).min(cap_ms);
    let delay = if ceiling == u64::MAX {
        sample
    } else {
        sample % (ceiling + 1)
    };
    std::time::Duration::from_millis(delay)
}

/// Creates a graph node, sidecar row, optional vector, and changelog row atomically.
pub async fn create_node(store: &PostgresGraphStore, intent: NodeWriteIntent) -> Result<Uuid> {
    validate_node_scope(&intent)?;
    let (tenant_id, contact_id) = runtime_ids_for_node(store, &intent)?;
    // Seal restricted content before opening the transaction: the KMS round trip
    // must not run while a database transaction is held.
    let prepared = prepare_node_fields(store, &intent, tenant_id).await?;
    let mut conn = store.begin_required().await?;
    let uid = write_created_node(
        store,
        conn.as_mut(),
        &intent,
        &prepared,
        tenant_id,
        contact_id,
    )
    .await?;
    conn.commit().await?;
    Ok(uid)
}

/// Creates a graph node, sidecar row, optional vector, and changelog row in a caller-owned tx.
pub async fn create_node_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: NodeWriteIntent,
) -> Result<Uuid> {
    validate_node_scope(&intent)?;
    let (tenant_id, contact_id) = runtime_ids_for_node(store, &intent)?;
    // Seal up front (async KMS + CPU, no rows touched) so the caller's transaction
    // does no encryption work while holding locks.
    let prepared = prepare_node_fields(store, &intent, tenant_id).await?;
    write_created_node(store, conn, &intent, &prepared, tenant_id, contact_id).await
}

/// Creates several graph nodes, sidecar rows, optional vectors, and changelog rows atomically.
///
/// All nodes are written inside one transaction: the `node_index` rows are
/// inserted with a single `UNNEST` multi-row statement (JSON travels as `TEXT[]`
/// cast to `JSONB`, mirroring the tenant-knowledge batch inserts), every
/// embedding-bearing node's vector is upserted in one call, and one changelog
/// statement retains a row per node while incrementing the storage-partition
/// generation once. Returns the created uids in input order.
pub async fn bulk_create_nodes(
    store: &PostgresGraphStore,
    mut intents: Vec<NodeWriteIntent>,
) -> Result<Vec<Uuid>> {
    if intents.is_empty() {
        return Ok(Vec::new());
    }
    for intent in &intents {
        validate_node_scope(intent)?;
    }
    // Preserve the caller-visible return order (uids in input order); the sort
    // below is a purely internal lock-ordering optimization.
    let input_order_uids = intents.iter().map(|intent| intent.uid).collect::<Vec<_>>();
    // Acquire row locks in a deterministic (uid-sorted) order across every
    // concurrent writer. Shared entity nodes are reused across documents, so two
    // documents ingesting in parallel would otherwise INSERT the same uids in
    // different array orders and deadlock; sorting removes that cycle. It also
    // fixes the order of the per-intent changelog writes below.
    intents.sort_by_key(|intent| intent.uid);

    let count = intents.len();
    let mut uids = Vec::with_capacity(count);
    let mut labels = Vec::with_capacity(count);
    let mut storage_partition_ids: Vec<Option<String>> = Vec::with_capacity(count);
    let mut user_ids: Vec<Option<String>> = Vec::with_capacity(count);
    let mut tenant_ids = Vec::with_capacity(count);
    let mut contact_ids: Vec<Option<Uuid>> = Vec::with_capacity(count);
    let mut names = Vec::with_capacity(count);
    let mut pii_classes = Vec::with_capacity(count);
    let mut barriers: Vec<Option<String>> = Vec::with_capacity(count);
    let mut confidences: Vec<Option<f64>> = Vec::with_capacity(count);
    let mut reference_counts = Vec::with_capacity(count);
    let mut valid_froms = Vec::with_capacity(count);
    let mut properties = Vec::with_capacity(count);
    // Redaction-safe properties (placeholder for sealed rows) reused for each
    // node's changelog outbox row so the changelog never carries the secret.
    let mut changelog_properties: Vec<Value> = Vec::with_capacity(count);
    let mut data_subject_ids = Vec::with_capacity(count);
    let mut content_sealed: Vec<Option<Vec<u8>>> = Vec::with_capacity(count);
    let mut vector_items = Vec::new();
    let mut vector_state_seeds = Vec::new();
    // Intent-prep phase: encrypt restricted/PHI content here, before the
    // deadlock-retry transaction loop below. This does async KMS + CPU work and
    // touches no database rows, so it neither changes the uid-sorted lock order
    // nor runs inside a retry (the sealed bytes are captured in these arrays and
    // reused verbatim on every retry — content is never re-encrypted).
    let runtime_ids = intents
        .iter()
        .map(|intent| runtime_ids_for_node(store, intent))
        .collect::<Result<Vec<_>>>()?;
    let runtime_tenant_ids = runtime_ids
        .iter()
        .map(|(tenant_id, _)| *tenant_id)
        .collect::<Vec<_>>();
    let prepared_batch = prepare_node_fields_batch(store, &intents, &runtime_tenant_ids).await?;
    for ((intent, (tenant_id, contact_id)), prepared) in
        intents.iter().zip(runtime_ids).zip(prepared_batch)
    {
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
        data_subject_ids.push(intent.data_subject_id);
        names.push(prepared.name);
        pii_classes.push(intent.pii_class.as_str().to_string());
        barriers.push(
            intent
                .barrier
                .as_ref()
                .map(InformationBarrierId::as_str)
                .map(ToOwned::to_owned),
        );
        confidences.push(intent.confidence);
        reference_counts.push(reference_count_from_properties(&intent.properties));
        valid_froms.push(intent.valid_from);
        properties.push(serde_json::to_string(&prepared.properties)?);
        changelog_properties.push(prepared.properties);
        content_sealed.push(prepared.content_sealed);
    }

    // The whole write is idempotent (node INSERT is `ON CONFLICT DO NOTHING`
    // and the changelog rows roll back with a failed attempt), so a deadlock
    // victim is retried under a fresh transaction rather than propagated.
    let mut attempt = 0_u32;
    loop {
        let outcome: Result<()> = async {
            let mut conn = store.begin_required().await?;
            install_write_barriers(
                conn.as_mut(),
                intents.iter().filter_map(|intent| intent.barrier.clone()),
            )
            .await?;
            // Node uids are identity-derived (content hash for knowledge chunks,
            // fact identity for memory facts), so a uid conflict means another
            // writer created the same entity concurrently — e.g. two documents
            // sharing chunk content ingested in parallel. Skipping is the correct
            // outcome; failing aborted whole corpus syncs on the first shared chunk.
            sqlx::query(
                r#"
                INSERT INTO moa.node_index
                    (uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class,
                     barrier, confidence, reference_count, valid_from, properties_summary,
                     data_subject_id, content_sealed)
                SELECT n.uid, n.label, n.storage_partition_id, n.user_id, n.tenant_id, n.contact_id,
                       n.name, n.pii_class, n.barrier, n.confidence, n.reference_count, n.valid_from,
                       n.properties::JSONB, n.data_subject_id, n.content_sealed
                FROM UNNEST(
                    $1::UUID[], $2::TEXT[], $3::TEXT[], $4::TEXT[], $5::UUID[], $6::UUID[], $7::TEXT[],
                    $8::TEXT[], $9::DOUBLE PRECISION[], $10::BIGINT[], $11::TIMESTAMPTZ[], $12::TEXT[],
                    $13::UUID[], $14::BYTEA[], $15::TEXT[]
                ) AS n(uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class,
                       confidence, reference_count, valid_from, properties, data_subject_id, content_sealed,
                       barrier)
                ON CONFLICT (uid) DO NOTHING
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
            .bind(&data_subject_ids)
            .bind(&content_sealed)
            .bind(&barriers)
            .execute(conn.as_mut())
            .await?;

            if !vector_items.is_empty() {
                for (storage_partition_id, embedding_model, embedding_model_version) in
                    &vector_state_seeds
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

            let changelog_records = intents
                .iter()
                .zip(&changelog_properties)
                .map(|(intent, changelog_props)| {
                    create_changelog(intent, changelog_props, None)
                })
                .collect::<Vec<_>>();
            write_batch_and_bump(conn.as_mut(), &changelog_records).await?;

            conn.commit().await?;
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => break,
            Err(error) if is_deadlock(&error) && attempt < MAX_DEADLOCK_RETRIES => {
                attempt += 1;
                deadlock_backoff(attempt).await;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(input_order_uids)
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
    let (tenant_id, contact_id) = runtime_ids_for_node(store, &new)?;
    // Seal the replacement's restricted content before any locking SQL runs.
    let prepared = prepare_node_fields(store, &new, tenant_id).await?;
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
    insert_node_index(&mut *conn, &new, &prepared, tenant_id, contact_id).await?;
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
        return Err(Error::Conflict(
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
    write_and_bump(
        &mut *conn,
        create_changelog(&new, &prepared.properties, Some(old_change)),
    )
    .await?;

    Ok(new.uid)
}

/// Soft-invalidates one graph node and removes its vector projection atomically.
pub async fn invalidate_node(store: &PostgresGraphStore, uid: Uuid, reason: &str) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), uid)
        .await?
        .ok_or(Error::NotFound(uid))?;
    if old.valid_to.is_some() {
        return Err(Error::BiTemporal(format!("{uid} is already invalidated")));
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
    Ok(())
}

/// Soft-invalidates a bounded node batch and removes its vector projections atomically.
///
/// Caller input is capped before de-duplication, then sorted by UID so row locks,
/// returned UIDs, and changelog rows are deterministic. Missing nodes are
/// omitted. If any visible node is already invalidated, the transaction fails
/// before changing another node.
pub async fn bulk_invalidate_nodes(
    store: &PostgresGraphStore,
    uids: &[Uuid],
    reason: &str,
) -> Result<Vec<Uuid>> {
    if uids.len() > MAX_BULK_INVALIDATE_NODES {
        return Err(Error::Conflict(format!(
            "bulk node invalidation accepts at most {MAX_BULK_INVALIDATE_NODES} UIDs, got {}",
            uids.len()
        )));
    }
    let mut requested_uids = uids.to_vec();
    requested_uids.sort_unstable();
    requested_uids.dedup();
    if requested_uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = store.begin_required().await?;
    let stored_nodes = fetch_stored_nodes(conn.as_mut(), &requested_uids).await?;
    if let Some((uid, _)) = stored_nodes
        .iter()
        .find(|(_, node)| node.valid_to.is_some())
    {
        return Err(Error::BiTemporal(format!("{uid} is already invalidated")));
    }
    let invalidated_uids = stored_nodes.iter().map(|(uid, _)| *uid).collect::<Vec<_>>();
    if invalidated_uids.is_empty() {
        conn.commit().await?;
        return Ok(Vec::new());
    }

    let now = Utc::now();
    let (actor_id, actor_kind) = mutation_actor(store);
    let updated = sqlx::query(
        r#"
        UPDATE moa.node_index
        SET valid_to = $1,
            invalidated_at = $1,
            invalidated_by = $2,
            invalidated_reason = $3
        WHERE uid = ANY($4::UUID[])
          AND valid_to IS NULL
        "#,
    )
    .bind(now)
    .bind(actor_id.as_deref().and_then(actor_uuid))
    .bind(reason)
    .bind(&invalidated_uids)
    .execute(conn.as_mut())
    .await?;
    if updated.rows_affected() != invalidated_uids.len() as u64 {
        return Err(Error::BiTemporal(
            "bulk node invalidation lost an active row lock".to_string(),
        ));
    }
    close_incident_edges_batch(conn.as_mut(), &invalidated_uids, now).await?;
    if let Some(vector) = store.vector() {
        vector
            .delete_in_tx(conn.as_mut(), &invalidated_uids)
            .await?;
    }

    let changelog_records = stored_nodes
        .into_iter()
        .map(|(uid, old)| ChangelogRecord {
            storage_partition_id: old.storage_partition_id,
            contact_id: old.contact_id,
            scope: old.scope,
            actor_id: actor_id.clone(),
            actor_kind: actor_kind.clone(),
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
        })
        .collect::<Vec<_>>();
    write_batch_and_bump(conn.as_mut(), &changelog_records).await?;

    conn.commit().await?;
    Ok(invalidated_uids)
}

/// Closes one active graph node into an already-existing replacement node atomically.
pub(crate) async fn close_existing_node_with_supersession(
    store: &PostgresGraphStore,
    intent: ExistingSupersessionIntent,
) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.old_uid)
        .await?
        .ok_or(Error::NotFound(intent.old_uid))?;
    let replacement = fetch_stored_node(conn.as_mut(), intent.replacement_uid)
        .await?
        .ok_or(Error::NotFound(intent.replacement_uid))?;
    if old.valid_to.is_some() {
        return Err(Error::BiTemporal(format!(
            "{} is already invalidated",
            intent.old_uid
        )));
    }
    if replacement.valid_to.is_some() {
        return Err(Error::BiTemporal(format!(
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
    Ok(())
}

/// Closes one active graph node without a replacement, at caller-provided instants.
///
/// Bitemporal close only: the node row, its incident edges, and its vector rows
/// are closed or removed, and a changelog record is written — history and as-of
/// reads keep working. Returns `false` without writing when the node is already
/// closed, so scheduled passes rerun idempotently at the same `now`.
pub(crate) async fn expire_node_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: NodeExpiryIntent,
) -> Result<bool> {
    let old = fetch_stored_node(&mut *conn, intent.uid)
        .await?
        .ok_or(Error::NotFound(intent.uid))?;
    if old.valid_to.is_some() {
        return Ok(false);
    }

    close_node_index(
        &mut *conn,
        intent.uid,
        intent.valid_to,
        intent.invalidated_at,
        actor_uuid(&intent.actor_id),
        &intent.reason,
    )
    .await?;
    close_incident_edges(&mut *conn, intent.uid, intent.valid_to).await?;
    if let Some(vector) = store.vector() {
        vector.delete_in_tx(&mut *conn, &[intent.uid]).await?;
    }
    write_and_bump(
        &mut *conn,
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

    Ok(true)
}

/// Replaces one active graph node's mutable content atomically.
pub(crate) async fn update_node_content(
    store: &PostgresGraphStore,
    intent: NodeContentUpdateIntent,
) -> Result<()> {
    if !intent.properties.is_object() {
        return Err(Error::Conflict(
            "node properties must be a JSON object".to_string(),
        ));
    }

    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(Error::NotFound(intent.uid))?;
    if old.valid_to.is_some() {
        return Err(Error::BiTemporal(format!("{} is not active", intent.uid)));
    }
    let barrier = old
        .barrier
        .as_deref()
        .map(InformationBarrierId::parse)
        .transpose()
        .map_err(|error| Error::Conflict(error.to_string()))?;
    let replacement = NodeWriteIntent {
        uid: intent.uid,
        label: old.label,
        storage_partition_id: old.storage_partition_id.clone(),
        contact_id: old.contact_id.clone(),
        data_subject_id: old.data_subject_id,
        name: intent.name.clone(),
        pii_class: old.pii_class,
        barrier,
        confidence: intent.confidence,
        valid_from: old.valid_from,
        properties: intent.properties.clone(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        scope: old.scope.clone(),
        actor_id: intent.actor_id.clone(),
        actor_kind: intent.actor_kind.clone(),
    };
    let prepared = prepare_node_fields(store, &replacement, old.tenant_id).await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.node_index
        SET name = $1,
            properties_summary = $2,
            content_sealed = $3,
            confidence = COALESCE($4, confidence)
        WHERE uid = $5
          AND valid_to IS NULL
        "#,
    )
    .bind(&prepared.name)
    .bind(&prepared.properties)
    .bind(prepared.content_sealed.as_deref())
    .bind(intent.confidence)
    .bind(intent.uid)
    .execute(conn.as_mut())
    .await?;
    if result.rows_affected() == 0 {
        return Err(Error::BiTemporal(format!("{} is not active", intent.uid)));
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
                "after": prepared.properties,
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
            base_confidence = NULL,
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
pub(crate) async fn upsert_node_embedding(
    store: &PostgresGraphStore,
    intent: NodeEmbeddingIntent,
) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let node = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(Error::NotFound(intent.uid))?;
    if node.valid_to.is_some() {
        return Err(Error::BiTemporal(format!("{} is not active", intent.uid)));
    }
    if node.pii_class.is_sealed() {
        return Err(Error::SealedEmbedding);
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
                pii_class: node.pii_class,
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
        .ok_or(Error::NotFound(uid))?;
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
    Ok(())
}

/// Creates a graph edge and changelog row atomically.
pub async fn create_edge(store: &PostgresGraphStore, intent: EdgeWriteIntent) -> Result<Uuid> {
    let uid = intent.uid;
    bulk_create_edges(store, vec![intent]).await?;
    Ok(uid)
}

/// Creates graph edges and their changelog rows in one scoped transaction.
///
/// Duplicate UIDs retain their first input occurrence. Only rows inserted by
/// this call are returned and emitted to the changelog, so replaying a batch is
/// a true no-op. Endpoint validation and row locking happen before either
/// mutation statement.
pub async fn bulk_create_edges(
    store: &PostgresGraphStore,
    intents: Vec<EdgeWriteIntent>,
) -> Result<Vec<Uuid>> {
    let mut seen = HashSet::with_capacity(intents.len());
    let intents = intents
        .into_iter()
        .filter(|intent| seen.insert(intent.uid))
        .collect::<Vec<_>>();
    if intents.is_empty() {
        return Ok(Vec::new());
    }

    let writes = intents
        .into_iter()
        .map(|intent| {
            validate_edge_scope(&intent)?;
            let (tenant_id, contact_id) = runtime_ids_from_parts(
                store,
                intent.storage_partition_id.as_deref(),
                intent.contact_id.as_deref(),
                "edges",
            )?;
            Ok(ScopedEdgeWrite {
                intent,
                tenant_id,
                contact_id,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // The mutation is idempotent, so a deadlock victim can retry on a fresh
    // scoped transaction without creating duplicate edges or outbox rows.
    let mut attempt = 0_u32;
    loop {
        let outcome: Result<Vec<Uuid>> = async {
            let mut conn = store.begin_required().await?;
            validate_edge_endpoints_batch(conn.as_mut(), &writes).await?;
            let inserted = insert_edge_index_batch(conn.as_mut(), &writes).await?;
            if !inserted.is_empty() {
                let write_by_uid = writes
                    .iter()
                    .map(|write| (write.intent.uid, write))
                    .collect::<HashMap<_, _>>();
                let changelog_records = inserted
                    .iter()
                    .map(|uid| edge_changelog(&write_by_uid[uid].intent))
                    .collect::<Vec<_>>();
                write_batch_and_bump(conn.as_mut(), &changelog_records).await?;
            }
            conn.commit().await?;
            Ok(inserted)
        }
        .await;
        match outcome {
            Ok(inserted) => return Ok(inserted),
            Err(error) if is_deadlock(&error) && attempt < MAX_DEADLOCK_RETRIES => {
                attempt += 1;
                deadlock_backoff(attempt).await;
            }
            Err(error) => return Err(error),
        }
    }
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
    write_and_bump(&mut *conn, edge_changelog(&intent)).await?;

    Ok(intent.uid)
}

#[cfg(test)]
mod tests {
    use super::full_jitter_delay;

    #[test]
    fn full_jitter_delay_is_bounded_and_sample_driven() {
        // Pins: deadlock peers do not share a deterministic fixed delay, and
        // the exponential ceiling is capped on later attempts.
        assert_eq!(full_jitter_delay(4, 128, 0, 0).as_millis(), 0);
        assert_eq!(full_jitter_delay(4, 128, 0, 4).as_millis(), 4);
        assert_eq!(full_jitter_delay(4, 128, 5, 128).as_millis(), 128);
        assert!(full_jitter_delay(4, 128, 40, u64::MAX).as_millis() <= 128);
    }
}
