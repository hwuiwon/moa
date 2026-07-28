//! Atomic graph write protocol for relational rows, vectors, and changelog records.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use moa_core::types::{memory::InformationBarrierId, security::SensitivityClass};
use moa_crypto::{EncryptionContext, EncryptionRequest};
use moa_memory_vector::{VECTOR_DIMENSION, VectorItem, VectorStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{
    Error, PostgresGraphStore, Result,
    changelog::{ChangelogRecord, write_and_bump},
    edge::{EdgeLabel, EdgeWriteIntent},
    node::{
        ExistingSupersessionIntent, NodeContentUpdateIntent, NodeEmbeddingIntent, NodeExpiryIntent,
        NodeLabel, NodeReinforcementIntent, NodeWriteIntent,
    },
};

/// Maximum attempts for a write transaction that aborts on a Postgres
/// serialization deadlock (SQLSTATE `40P01`).
const MAX_DEADLOCK_RETRIES: u32 = 5;

/// Placeholder written into the indexed plaintext `name` column of a
/// restricted/PHI node, so the generated `name_tsv` full-text index only ever
/// sees this token and never the sealed secret.
pub(crate) const REDACTED_NAME_PLACEHOLDER: &str = "[RESTRICTED]";

/// Placeholder written into the indexed plaintext `properties_summary` column of
/// a restricted/PHI node, keeping the generated `properties_tsv` index free of
/// the sealed secret.
fn redacted_properties() -> Value {
    json!({ "redacted": true })
}

/// Version of the plaintext document stored inside `content_sealed`.
pub(crate) const SEALED_CONTENT_VERSION: u8 = 1;

/// Complete mutable content encrypted as one atomic document.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SealedNodeContent {
    /// Payload format version.
    pub(crate) version: u8,
    /// Human-readable node name.
    pub(crate) name: String,
    /// Dynamic node properties.
    pub(crate) properties: Value,
}

/// The indexed plaintext columns plus any sealed ciphertext for one node write.
///
/// Produced by [`prepare_node_fields`] in the intent-prep phase and consumed by
/// [`insert_node_index`]. For `none`/`pii` nodes it carries the real name and
/// properties with no ciphertext; for `restricted`/`phi` nodes it carries the
/// redaction placeholders plus the sealed blobs and flags the embedding for
/// exclusion.
struct PreparedNodeFields {
    /// Value bound into the indexed plaintext `name` column.
    name: String,
    /// Value bound into the indexed plaintext `properties_summary` column.
    properties: Value,
    /// Envelope ciphertext of the complete content document, or `None`.
    content_sealed: Option<Vec<u8>>,
}

/// Seals one node's restricted/PHI content ahead of the SQL transaction.
///
/// This is the intent-prep step: it performs async KMS + AEAD work only and
/// touches no database rows, so it can run before `begin_required()` and must
/// never participate in row-lock ordering or the bulk deadlock-retry loop. For
/// `none`/`pii` nodes it is a cheap identity (no crypto). For `restricted`/`phi`
/// nodes it seals one versioned `{name, properties}` payload under the node's
/// explicit `(tenant, data_subject_id)` KEK and substitutes redaction
/// placeholders into the indexed plaintext columns. Restricted content with an
/// embedding is rejected rather than silently dropping caller input.
async fn prepare_node_fields(
    store: &PostgresGraphStore,
    intent: &NodeWriteIntent,
    tenant_id: Uuid,
) -> Result<PreparedNodeFields> {
    prepare_node_fields_batch(store, std::slice::from_ref(intent), &[tenant_id])
        .await?
        .pop()
        .ok_or_else(|| Error::Conflict("node preparation returned no fields".to_string()))
}

/// Prepares a node batch and performs one KMS call per `(tenant, subject)` group.
async fn prepare_node_fields_batch(
    store: &PostgresGraphStore,
    intents: &[NodeWriteIntent],
    tenant_ids: &[Uuid],
) -> Result<Vec<PreparedNodeFields>> {
    if intents.len() != tenant_ids.len() {
        return Err(Error::Conflict(
            "node preparation tenant cardinality mismatch".to_string(),
        ));
    }

    let mut prepared = intents
        .iter()
        .map(|intent| {
            if intent.pii_class.is_sealed() {
                None
            } else {
                Some(PreparedNodeFields {
                    name: intent.name.clone(),
                    properties: intent.properties.clone(),
                    content_sealed: None,
                })
            }
        })
        .collect::<Vec<_>>();
    let mut groups: BTreeMap<(Uuid, Uuid), Vec<(usize, EncryptionRequest)>> = BTreeMap::new();

    for (index, (intent, tenant_id)) in intents.iter().zip(tenant_ids).enumerate() {
        if !intent.pii_class.is_sealed() {
            continue;
        }
        if intent.embedding.is_some() {
            return Err(Error::SealedEmbedding);
        }
        let payload = serde_json::to_vec(&SealedNodeContent {
            version: SEALED_CONTENT_VERSION,
            name: intent.name.clone(),
            properties: intent.properties.clone(),
        })?;
        let context = EncryptionContext::new(
            *tenant_id,
            intent.data_subject_id,
            intent.uid.to_string(),
            intent.pii_class.as_str(),
        );
        groups
            .entry((*tenant_id, intent.data_subject_id))
            .or_default()
            .push((index, EncryptionRequest::new(payload, context)));
    }

    for requests in groups.into_values() {
        let encryption_requests = requests
            .iter()
            .map(|(_, request)| request.clone())
            .collect::<Vec<_>>();
        let ciphertexts =
            moa_crypto::encrypt_batch(store.kms().as_ref(), &encryption_requests).await?;
        for ((index, _), ciphertext) in requests.into_iter().zip(ciphertexts) {
            prepared[index] = Some(PreparedNodeFields {
                name: REDACTED_NAME_PLACEHOLDER.to_string(),
                properties: redacted_properties(),
                content_sealed: Some(ciphertext.to_bytes()),
            });
        }
    }

    prepared
        .into_iter()
        .map(|fields| {
            fields.ok_or_else(|| {
                Error::Conflict("sealed node preparation returned no fields".to_string())
            })
        })
        .collect()
}

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

/// Extends the transaction-local read clearances with barriers authored by this write.
///
/// PostgreSQL applies the restrictive `node_index` SELECT policy while resolving
/// `INSERT .. ON CONFLICT`, so a writer must be able to see the barrier-tagged
/// row it is inserting or de-duplicating. The value remains transaction-local
/// and is derived only from validated write intents; it never grants a caller a
/// durable or request-wide read clearance.
async fn install_write_barriers(
    conn: &mut PgConnection,
    barriers: impl IntoIterator<Item = InformationBarrierId>,
) -> Result<()> {
    let mut barriers = barriers
        .into_iter()
        .map(|barrier| barrier.as_str().to_owned())
        .collect::<Vec<_>>();
    barriers.sort_unstable();
    barriers.dedup();
    if barriers.is_empty() {
        return Ok(());
    }

    sqlx::query(
        r#"
        SELECT pg_catalog.set_config(
            'moa.cleared_barriers',
            array_to_string(
                ARRAY(
                    SELECT DISTINCT barrier
                    FROM unnest(moa.current_cleared_barriers() || $1::TEXT[])
                        AS authored(barrier)
                    ORDER BY barrier
                ),
                ','
            ),
            true
        )
        "#,
    )
    .bind(&barriers)
    .execute(conn)
    .await?;
    Ok(())
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

/// Writes one prepared node row, its optional vector, and its changelog row.
///
/// Restricted/PHI embeddings are rejected during preparation so sealed content
/// is neither full-text nor semantically searchable.
async fn write_created_node(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    intent: &NodeWriteIntent,
    prepared: &PreparedNodeFields,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
) -> Result<Uuid> {
    insert_node_index(&mut *conn, intent, prepared, tenant_id, contact_id).await?;
    if let Some(item) = vector_item_from_intent(intent)? {
        ensure_storage_partition_embedder_state(
            &mut *conn,
            intent.storage_partition_id.as_deref(),
            &item.embedding_model,
            item.embedding_model_version,
        )
        .await?;
        let vector = require_vector_store(store)?;
        vector
            .upsert_in_tx(&mut *conn, std::slice::from_ref(&item))
            .await?;
    }
    write_and_bump(
        &mut *conn,
        create_changelog(intent, &prepared.properties, None),
    )
    .await?;

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

            for (intent, changelog_props) in intents.iter().zip(&changelog_properties) {
                write_and_bump(
                    conn.as_mut(),
                    create_changelog(intent, changelog_props, None),
                )
                .await?;
            }

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
pub(crate) async fn expire_node(
    store: &PostgresGraphStore,
    intent: NodeExpiryIntent,
) -> Result<bool> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), intent.uid)
        .await?
        .ok_or(Error::NotFound(intent.uid))?;
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
    // `create_edge_in_conn` is idempotent (it no-ops when the edge already
    // exists), so a deadlock victim — e.g. an edge onto a shared entity node
    // whose partition changelog row another writer holds — is retried on a fresh
    // transaction rather than aborting the ingesting document.
    let mut attempt = 0_u32;
    loop {
        let outcome: Result<Uuid> = async {
            let mut conn = store.begin_required().await?;
            let uid = create_edge_in_conn(store, conn.as_mut(), intent.clone()).await?;
            conn.commit().await?;
            Ok(uid)
        }
        .await;
        match outcome {
            Ok(uid) => return Ok(uid),
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
        .map_err(Error::from)
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
        .ok_or(Error::NotFound(intent.start_uid))?;
    let end = fetch_stored_node(conn, intent.end_uid)
        .await?
        .ok_or(Error::NotFound(intent.end_uid))?;
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
        return Err(Error::Conflict(
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
        return Err(Error::Conflict(
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
    let expected = expected_scope_tier(storage_partition_id, contact_id)
        .ok_or_else(|| Error::Conflict("contact scope requires storage partition".to_string()))?;
    if scope == expected {
        Ok(())
    } else {
        Err(Error::Conflict(format!(
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

/// Returns a [`Error::Conflict`] with `message` when the two scope triples differ.
fn ensure_same_scope(a: &impl ScopeTriple, b: &impl ScopeTriple, message: &str) -> Result<()> {
    if a.scope_triple() == b.scope_triple() {
        Ok(())
    } else {
        Err(Error::Conflict(message.to_string()))
    }
}

/// Inserts one `moa.node_index` row from already-prepared fields.
///
/// Pure SQL: encryption happened earlier in [`prepare_node_fields`], so this
/// binds the indexed plaintext (`prepared.name`/`prepared.properties`, which are
/// redaction placeholders for sealed rows) alongside the sealed ciphertext
/// columns. `reference_count` is derived from the real `intent.properties`
/// because it is routing metadata, not sensitive content.
async fn insert_node_index(
    conn: &mut PgConnection,
    intent: &NodeWriteIntent,
    prepared: &PreparedNodeFields,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
) -> Result<()> {
    install_write_barriers(conn, intent.barrier.iter().cloned()).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class, barrier,
             confidence, reference_count, valid_from, properties_summary, data_subject_id, content_sealed)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(intent.uid)
    .bind(intent.label.as_str())
    .bind(intent.storage_partition_id.as_deref())
    .bind(intent.contact_id.as_deref())
    .bind(tenant_id)
    .bind(contact_id)
    .bind(&prepared.name)
    .bind(intent.pii_class.as_str())
    .bind(intent.barrier.as_ref().map(InformationBarrierId::as_str))
    .bind(intent.confidence)
    .bind(reference_count_from_properties(&intent.properties))
    .bind(intent.valid_from)
    .bind(&prepared.properties)
    .bind(intent.data_subject_id)
    .bind(prepared.content_sealed.as_deref())
    .execute(conn)
    .await?;
    Ok(())
}

fn runtime_ids_for_node(
    store: &PostgresGraphStore,
    intent: &NodeWriteIntent,
) -> Result<(Uuid, Option<Uuid>)> {
    let (tenant_id, contact_id) = runtime_ids_from_parts(
        store,
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        "nodes",
    )?;
    let expected_subject_id = contact_id.unwrap_or(tenant_id);
    if intent.data_subject_id != expected_subject_id {
        return Err(Error::DataSubjectMismatch {
            actual: intent.data_subject_id,
            expected: expected_subject_id,
        });
    }
    Ok((tenant_id, contact_id))
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
        return Err(Error::Conflict(format!(
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
        Error::Conflict(format!(
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
        Err(Error::BiTemporal(format!("{uid} was already invalidated")))
    } else {
        Ok(())
    }
}

fn vector_item_from_intent(intent: &NodeWriteIntent) -> Result<Option<VectorItem>> {
    let Some(embedding) = intent.embedding.clone() else {
        return Ok(None);
    };
    if intent.pii_class.is_sealed() {
        return Err(Error::SealedEmbedding);
    }
    let Some(embedding_model) = intent.embedding_model.clone() else {
        return Err(Error::MissingEmbeddingMetadata);
    };
    let Some(embedding_model_version) = intent.embedding_model_version else {
        return Err(Error::MissingEmbeddingMetadata);
    };
    Ok(Some(VectorItem {
        uid: intent.uid,
        user_id: intent.contact_id.clone(),
        label: intent.label.as_str().to_string(),
        pii_class: intent.pii_class,
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
        Error::Conflict("embedding writes require storage partition state".to_string())
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
        Error::Conflict("embedding provided but no vector store is configured".to_string())
    })
}

/// Builds the create-node changelog record.
///
/// `properties` is the redaction-safe projection (`PreparedNodeFields::properties`):
/// for restricted/PHI nodes it is the placeholder, never the sealed secret, so
/// the append-only `graph_changelog` outbox (which also drives vector sync) never
/// carries plaintext restricted content.
fn create_changelog(
    intent: &NodeWriteIntent,
    properties: &Value,
    cause_change_id: Option<i64>,
) -> ChangelogRecord {
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
        payload: json!({ "after": properties }),
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
        SELECT label, storage_partition_id, user_id, tenant_id, data_subject_id, scope, pii_class,
               barrier, valid_from, valid_to, properties_summary
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
            return Err(Error::BiTemporal(format!(
                "supersession cycle detected while resolving {initial_uid}"
            )));
        }

        let stored = fetch_stored_node(conn, uid)
            .await?
            .ok_or(Error::NotFound(uid))?;
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
                return Err(Error::BiTemporal(format!(
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
        tenant_id: row.try_get("tenant_id")?,
        data_subject_id: row.try_get("data_subject_id")?,
        scope: row.try_get("scope")?,
        pii_class: pii_class_text.parse()?,
        barrier: row.try_get("barrier")?,
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
    tenant_id: Uuid,
    data_subject_id: Uuid,
    scope: String,
    pii_class: SensitivityClass,
    barrier: Option<String>,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    properties_summary: Option<Value>,
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
