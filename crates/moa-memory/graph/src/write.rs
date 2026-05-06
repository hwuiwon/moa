//! Atomic graph write protocol for AGE, sidecar rows, vectors, and changelog records.

use chrono::{DateTime, Utc};
use moa_memory_vector::{VectorItem, VectorStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::{
    GraphError, Result,
    age::AgeGraphStore,
    changelog::{ChangelogRecord, write_and_bump},
    cypher::{self, Cypher},
    edge::{EdgeLabel, EdgeWriteIntent},
    node::{NodeLabel, NodeWriteIntent, PiiClass},
};

/// Creates a graph node, sidecar row, optional vector, and changelog row atomically.
pub async fn create_node(store: &AgeGraphStore, intent: NodeWriteIntent) -> Result<Uuid> {
    let mut conn = store.begin_required().await?;
    let uid = create_node_in_conn(store, conn.as_mut(), intent).await?;
    conn.commit().await?;
    Ok(uid)
}

/// Creates a graph node, sidecar row, optional vector, and changelog row in a caller-owned tx.
pub async fn create_node_in_conn(
    store: &AgeGraphStore,
    conn: &mut PgConnection,
    intent: NodeWriteIntent,
) -> Result<Uuid> {
    validate_node_scope(&intent)?;
    let vector_item = vector_item_from_intent(&intent)?;
    let created_at = Utc::now();
    let params = node_params(&intent, created_at);

    node_create_template(intent.label)
        .execute(&params)
        .execute(&mut *conn)
        .await
        .map_err(|error| GraphError::Cypher(error.to_string()))?;
    insert_node_index(&mut *conn, &intent).await?;
    if let Some(item) = vector_item.as_ref() {
        let vector = require_vector_store(store)?;
        vector
            .upsert_in_tx(&mut *conn, std::slice::from_ref(item))
            .await?;
    }
    write_and_bump(&mut *conn, create_changelog(&intent, None)).await?;

    Ok(intent.uid)
}

/// Supersedes one active graph node with a replacement node atomically.
pub async fn supersede_node(
    store: &AgeGraphStore,
    old_uid: Uuid,
    new: NodeWriteIntent,
) -> Result<Uuid> {
    validate_node_scope(&new)?;
    let vector_item = vector_item_from_intent(&new)?;
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), old_uid)
        .await?
        .ok_or(GraphError::NotFound(old_uid))?;
    if old.valid_to.is_some() {
        return Err(GraphError::BiTemporal(format!(
            "{old_uid} is already invalidated"
        )));
    }
    if new.valid_from <= old.valid_from {
        return Err(GraphError::BiTemporal(
            "new.valid_from must follow old.valid_from".to_string(),
        ));
    }

    let now = Utc::now();
    let mut params = node_params(&new, now);
    let object = params
        .as_object_mut()
        .ok_or_else(|| GraphError::Conflict("node params must be an object".to_string()))?;
    object.insert("old_uid".to_string(), json!(old_uid.to_string()));
    object.insert("valid_to".to_string(), json!(new.valid_from.to_rfc3339()));
    object.insert("invalidated_at".to_string(), json!(now.to_rfc3339()));
    object.insert("actor".to_string(), json!(new.actor_id.clone()));
    object.insert("edge_uid".to_string(), json!(Uuid::now_v7().to_string()));

    cypher::node::SUPERSEDE
        .execute(&params)
        .execute(conn.as_mut())
        .await
        .map_err(|error| GraphError::Cypher(error.to_string()))?;
    close_node_index(
        conn.as_mut(),
        old_uid,
        new.valid_from,
        now,
        actor_uuid(&new.actor_id),
        "superseded",
    )
    .await?;
    insert_node_index(conn.as_mut(), &new).await?;

    if let Some(vector) = store.vector() {
        vector.delete_in_tx(conn.as_mut(), &[old_uid]).await?;
        if let Some(item) = vector_item.as_ref() {
            vector
                .upsert_in_tx(conn.as_mut(), std::slice::from_ref(item))
                .await?;
        }
    } else if vector_item.is_some() {
        return Err(GraphError::Conflict(
            "embedding provided but no vector store is configured".to_string(),
        ));
    }

    let old_change = write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            workspace_id: old.workspace_id.clone(),
            user_id: old.user_id.clone(),
            scope: old.scope.clone(),
            actor_id: Some(new.actor_id.clone()),
            actor_kind: new.actor_kind.clone(),
            op: "supersede".to_string(),
            target_kind: "node".to_string(),
            target_label: old.label.as_str().to_string(),
            target_uid: old_uid,
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
    write_and_bump(conn.as_mut(), create_changelog(&new, Some(old_change))).await?;

    conn.commit().await?;
    Ok(new.uid)
}

/// Soft-invalidates one graph node and removes its vector projection atomically.
pub async fn invalidate_node(store: &AgeGraphStore, uid: Uuid, reason: &str) -> Result<()> {
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
    cypher::node::INVALIDATE
        .execute(&json!({
            "uid": uid.to_string(),
            "now": now.to_rfc3339(),
            "actor": actor_id.clone().unwrap_or_default(),
            "reason": reason,
        }))
        .execute(conn.as_mut())
        .await
        .map_err(|error| GraphError::Cypher(error.to_string()))?;
    close_node_index(
        conn.as_mut(),
        uid,
        now,
        now,
        actor_id.as_deref().and_then(actor_uuid),
        reason,
    )
    .await?;
    if let Some(vector) = store.vector() {
        vector.delete_in_tx(conn.as_mut(), &[uid]).await?;
    }
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            workspace_id: old.workspace_id,
            user_id: old.user_id,
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

/// Hard-purges one graph node while preserving a redacted audit changelog row.
pub async fn hard_purge(store: &AgeGraphStore, uid: Uuid, redaction_marker: &str) -> Result<()> {
    let mut conn = store.begin_required().await?;
    let old = fetch_stored_node(conn.as_mut(), uid)
        .await?
        .ok_or(GraphError::NotFound(uid))?;
    let (actor_id, actor_kind) = mutation_actor(store);
    let properties_hash = hash_properties(old.properties_summary.as_ref())?;

    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            workspace_id: old.workspace_id.clone(),
            user_id: old.user_id.clone(),
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
            audit_metadata: None,
            cause_change_id: None,
        },
    )
    .await?;
    delete_age_node(conn.as_mut(), old.label, uid).await?;
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

/// Creates an AGE edge and changelog row atomically.
pub async fn create_edge(store: &AgeGraphStore, intent: EdgeWriteIntent) -> Result<Uuid> {
    validate_edge_scope(&intent)?;
    let mut conn = store.begin_required().await?;
    edge_create_template(intent.label)
        .execute(&edge_params(&intent))
        .execute(conn.as_mut())
        .await
        .map_err(|error| GraphError::Cypher(error.to_string()))?;
    write_and_bump(
        conn.as_mut(),
        ChangelogRecord {
            workspace_id: intent.workspace_id.clone(),
            user_id: intent.user_id.clone(),
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

    conn.commit().await?;
    Ok(intent.uid)
}

fn validate_node_scope(intent: &NodeWriteIntent) -> Result<()> {
    validate_scope_shape(
        intent.workspace_id.as_deref(),
        intent.user_id.as_deref(),
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
        intent.workspace_id.as_deref(),
        intent.user_id.as_deref(),
        &intent.scope,
    )?;
    if !intent.properties.is_object() {
        return Err(GraphError::Conflict(
            "edge properties must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

fn validate_scope_shape(
    workspace_id: Option<&str>,
    user_id: Option<&str>,
    scope: &str,
) -> Result<()> {
    let expected = match (workspace_id, user_id) {
        (None, None) => "global",
        (Some(_), None) => "workspace",
        (Some(_), Some(_)) => "user",
        (None, Some(_)) => {
            return Err(GraphError::Conflict(
                "user scope requires workspace_id".to_string(),
            ));
        }
    };
    if scope == expected {
        Ok(())
    } else {
        Err(GraphError::Conflict(format!(
            "scope `{scope}` does not match computed scope `{expected}`"
        )))
    }
}

fn node_create_template(label: NodeLabel) -> &'static Cypher {
    match label {
        NodeLabel::Entity => &cypher::node::CREATE_ENTITY,
        NodeLabel::Concept => &cypher::node::CREATE_CONCEPT,
        NodeLabel::Decision => &cypher::node::CREATE_DECISION,
        NodeLabel::Incident => &cypher::node::CREATE_INCIDENT,
        NodeLabel::Lesson => &cypher::node::CREATE_LESSON,
        NodeLabel::Fact => &cypher::node::CREATE_FACT,
        NodeLabel::Source => &cypher::node::CREATE_SOURCE,
    }
}

fn edge_create_template(label: EdgeLabel) -> &'static Cypher {
    match label {
        EdgeLabel::RelatesTo => &cypher::edge::CREATE_RELATES_TO,
        EdgeLabel::DependsOn => &cypher::edge::CREATE_DEPENDS_ON,
        EdgeLabel::Supersedes => &cypher::edge::CREATE_SUPERSEDES,
        EdgeLabel::Contradicts => &cypher::edge::CREATE_CONTRADICTS,
        EdgeLabel::DerivedFrom => &cypher::edge::CREATE_DERIVED_FROM,
        EdgeLabel::MentionedIn => &cypher::edge::CREATE_MENTIONED_IN,
        EdgeLabel::Caused => &cypher::edge::CREATE_CAUSED,
        EdgeLabel::LearnedFrom => &cypher::edge::CREATE_LEARNED_FROM,
        EdgeLabel::AppliesTo => &cypher::edge::CREATE_APPLIES_TO,
    }
}

fn node_params(intent: &NodeWriteIntent, created_at: DateTime<Utc>) -> Value {
    json!({
        "uid": intent.uid.to_string(),
        "workspace_id": intent.workspace_id.clone().unwrap_or_default(),
        "user_id": intent.user_id.clone().unwrap_or_default(),
        "scope": intent.scope,
        "name": intent.name,
        "pii_class": intent.pii_class.as_str(),
        "valid_from": intent.valid_from.to_rfc3339(),
        "created_at": created_at.to_rfc3339(),
        "properties": intent.properties,
    })
}

fn edge_params(intent: &EdgeWriteIntent) -> Value {
    json!({
        "uid": intent.uid.to_string(),
        "start_uid": intent.start_uid.to_string(),
        "end_uid": intent.end_uid.to_string(),
        "workspace_id": intent.workspace_id.clone().unwrap_or_default(),
        "user_id": intent.user_id.clone().unwrap_or_default(),
        "scope": intent.scope,
        "properties": intent.properties,
    })
}

async fn insert_node_index(conn: &mut PgConnection, intent: &NodeWriteIntent) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, workspace_id, user_id, name, pii_class, confidence,
             valid_from, properties_summary)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(intent.uid)
    .bind(intent.label.as_str())
    .bind(intent.workspace_id.as_deref())
    .bind(intent.user_id.as_deref())
    .bind(&intent.name)
    .bind(intent.pii_class.as_str())
    .bind(intent.confidence)
    .bind(intent.valid_from)
    .bind(&intent.properties)
    .execute(conn)
    .await?;
    Ok(())
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
        workspace_id: intent.workspace_id.clone(),
        user_id: intent.user_id.clone(),
        label: intent.label.as_str().to_string(),
        pii_class: intent.pii_class.as_str().to_string(),
        embedding,
        embedding_model,
        embedding_model_version,
        valid_to: None,
    }))
}

fn require_vector_store(store: &AgeGraphStore) -> Result<&dyn VectorStore> {
    store.vector().ok_or_else(|| {
        GraphError::Conflict("embedding provided but no vector store is configured".to_string())
    })
}

fn create_changelog(intent: &NodeWriteIntent, cause_change_id: Option<i64>) -> ChangelogRecord {
    ChangelogRecord {
        workspace_id: intent.workspace_id.clone(),
        user_id: intent.user_id.clone(),
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

fn mutation_actor(store: &AgeGraphStore) -> (Option<String>, String) {
    store
        .scope()
        .and_then(|scope| scope.user_id())
        .map(|user_id| (Some(user_id.to_string()), "user".to_string()))
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
        SELECT label, workspace_id, user_id, scope, pii_class,
               valid_from, valid_to, properties_summary
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

async fn delete_age_node(conn: &mut PgConnection, label: NodeLabel, uid: Uuid) -> Result<()> {
    let vertex_table = age_table(label.as_str());
    for edge_label in [
        EdgeLabel::RelatesTo,
        EdgeLabel::DependsOn,
        EdgeLabel::Supersedes,
        EdgeLabel::Contradicts,
        EdgeLabel::DerivedFrom,
        EdgeLabel::MentionedIn,
        EdgeLabel::Caused,
        EdgeLabel::LearnedFrom,
        EdgeLabel::AppliesTo,
    ] {
        let sql = format!(
            r#"
            WITH victim AS (
                SELECT id
                FROM {vertex_table}
                WHERE moa.age_property(properties, 'uid') =
                      ('"' || $1 || '"')::ag_catalog.agtype
            )
            DELETE FROM {edge_table} edge_row
            USING victim
            WHERE edge_row.start_id = victim.id
               OR edge_row.end_id = victim.id
            "#,
            edge_table = age_table(edge_label.as_str()),
        );
        sqlx::query(&sql)
            .bind(uid.to_string())
            .execute(&mut *conn)
            .await?;
    }

    let sql = format!(
        r#"
        DELETE FROM {vertex_table}
        WHERE moa.age_property(properties, 'uid') =
              ('"' || $1 || '"')::ag_catalog.agtype
        "#
    );
    sqlx::query(&sql)
        .bind(uid.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

fn age_table(label: &str) -> String {
    format!(r#"moa_graph."{label}""#)
}

fn stored_node_from_row(row: sqlx::postgres::PgRow) -> Result<StoredNode> {
    let label_text: String = row.try_get("label")?;
    let pii_class_text: String = row.try_get("pii_class")?;
    Ok(StoredNode {
        label: label_text.parse()?,
        workspace_id: row.try_get("workspace_id")?,
        user_id: row.try_get("user_id")?,
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
    workspace_id: Option<String>,
    user_id: Option<String>,
    scope: String,
    pii_class: PiiClass,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    properties_summary: Option<Value>,
}
