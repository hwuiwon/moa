//! Node-row, vector, and changelog helpers used by graph write transactions.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use moa_core::types::{memory::InformationBarrierId, security::SensitivityClass};
use moa_memory_vector::{VECTOR_DIMENSION, VectorItem, VectorStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{
    Error, PostgresGraphStore, Result,
    changelog::{ChangelogRecord, write_and_bump},
    node::{NodeLabel, NodeWriteIntent},
};

use super::{scope::install_write_barriers, sealed::PreparedNodeFields};

/// Writes one prepared node row, its optional vector, and its changelog row.
///
/// Restricted/PHI embeddings are rejected during preparation so sealed content
/// is neither full-text nor semantically searchable.
pub(super) async fn write_created_node(
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

/// Inserts one `moa.node_index` row from already-prepared fields.
///
/// Pure SQL: encryption happened earlier in the sealed-content preparation step, so this
/// binds the indexed plaintext (`prepared.name`/`prepared.properties`, which are
/// redaction placeholders for sealed rows) alongside the sealed ciphertext
/// columns. `reference_count` is derived from the real `intent.properties`
/// because it is routing metadata, not sensitive content.
pub(super) async fn insert_node_index(
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

/// Extracts the positive reference-count projection used by node indexing.
pub(super) fn reference_count_from_properties(properties: &Value) -> i64 {
    properties
        .get("reference_count")
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .unwrap_or(0)
}

/// Closes one active node row while preserving its bitemporal history.
pub(super) async fn close_node_index(
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

/// Validates and converts an optional node embedding into its vector projection.
pub(super) fn vector_item_from_intent(intent: &NodeWriteIntent) -> Result<Option<VectorItem>> {
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

/// Seeds the storage partition's immutable vector-space metadata when absent.
pub(super) async fn ensure_storage_partition_embedder_state(
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
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                updated_at = now()
            WHERE moa.storage_partition_state.embedding_model IS NULL
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

/// Returns the configured vector backend required by an embedding-bearing write.
pub(super) fn require_vector_store(store: &PostgresGraphStore) -> Result<&dyn VectorStore> {
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
pub(super) fn create_changelog(
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

/// Derives changelog actor metadata from the graph store's request scope.
pub(super) fn mutation_actor(store: &PostgresGraphStore) -> (Option<String>, String) {
    store
        .scope()
        .and_then(|scope| scope.contact_id())
        .map(|contact_id| (Some(contact_id.to_string()), "contact".to_string()))
        .unwrap_or((None, "system".to_string()))
}

/// Parses an actor identifier when it is a UUID suitable for relational audit columns.
pub(super) fn actor_uuid(actor_id: &str) -> Option<Uuid> {
    Uuid::parse_str(actor_id).ok()
}

/// Hashes a node's properties for redacted hard-purge audit metadata.
pub(super) fn hash_properties(properties: Option<&Value>) -> Result<String> {
    let bytes = serde_json::to_vec(properties.unwrap_or(&Value::Null))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

/// Locks and returns one visible stored node.
pub(super) async fn fetch_stored_node(
    conn: &mut PgConnection,
    uid: Uuid,
) -> Result<Option<StoredNode>> {
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

/// Locks and returns every visible stored node in canonical UID order.
pub(super) async fn fetch_stored_nodes(
    conn: &mut PgConnection,
    uids: &[Uuid],
) -> Result<Vec<(Uuid, StoredNode)>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, label, storage_partition_id, user_id, tenant_id, data_subject_id, scope,
               pii_class, barrier, valid_from, valid_to, properties_summary
        FROM moa.node_index
        WHERE uid = ANY($1::UUID[])
        ORDER BY uid
        FOR UPDATE
        "#,
    )
    .bind(uids)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let uid = row.try_get("uid")?;
            Ok((uid, stored_node_from_row(row)?))
        })
        .collect()
}

/// Follows changelog supersession links until it locks the current active node.
pub(super) async fn fetch_current_supersession_target(
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

/// Decodes one locked node-index row into the write protocol's stored shape.
pub(super) fn stored_node_from_row(row: sqlx::postgres::PgRow) -> Result<StoredNode> {
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
/// Node fields required while validating and recording graph mutations.
pub(super) struct StoredNode {
    pub(super) label: NodeLabel,
    pub(super) storage_partition_id: Option<String>,
    pub(super) contact_id: Option<String>,
    pub(super) tenant_id: Uuid,
    pub(super) data_subject_id: Uuid,
    pub(super) scope: String,
    pub(super) pii_class: SensitivityClass,
    pub(super) barrier: Option<String>,
    pub(super) valid_from: DateTime<Utc>,
    pub(super) valid_to: Option<DateTime<Utc>>,
    pub(super) properties_summary: Option<Value>,
}
