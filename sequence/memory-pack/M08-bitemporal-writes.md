# Step M08 — Bi-temporal write protocol (atomic Postgres tx across graph + sidecar + vector + changelog)

_Implement the four write modes — create, supersede, soft-invalidate, hard-purge — as single atomic Postgres transactions that touch AGE, the sidecar, the vector store, and the changelog together, with workspace_state version bump to invalidate caches downstream._

## 1 What this step is about

Bi-temporal correctness lives in this prompt. Every fact has `valid_from`/`valid_to` (application time) and `created_at`/`invalidated_at` (transaction time). Supersession atomically closes the old node and creates a new one with a `SUPERSEDES` edge. Soft-invalidation closes the node without a replacement. Hard-purge deletes the AGE node and sidecar row but writes a redaction marker to the changelog so audit lineage survives.

## 2 Files to read

- M07 (`GraphStore` trait, Cypher templates, error type)
- M05 (`VectorStore::upsert/delete`)
- M06 (`changelog::write_and_bump`)
- M02 (`ScopedConn`)

## 3 Goal

`AgeGraphStore::{create_node, supersede_node, invalidate_node, hard_purge, create_edge}` implementations land. Each runs in **one** Postgres transaction. Embedding upsert/delete is delegated to a `VectorStore` injected at construction time (so the tx semantics depend on whether the vector backend is in-Postgres pgvector or external Turbopuffer).

## 4 Rules

- **All four modes go through `ScopedConn`** so RLS GUCs are set.
- **Single Postgres transaction** for: AGE Cypher + sidecar UPSERT + (pgvector embedding upsert if pgvector backend) + changelog INSERT + workspace_state bump.
- **Turbopuffer backend**: vector ops happen *after* the Postgres commit, with idempotent retries. If Turbopuffer fails, a saga compensator (M27 covers production-grade) re-tries from the changelog. For v1 we raise an error and rely on the changelog as the source of truth for re-projection.
- **Supersession invariants**: old node `valid_to` must equal new node `valid_from`; both must be > old `valid_from`.
- **Hard-purge writes a redacted changelog row** before deleting; the row's `payload` carries hashes of the original content for audit reconstruction without preserving PII.
- **`pii_class` propagates through every layer** so RLS clearance gates apply consistently.

## 5 Tasks

### 5a `NodeWriteIntent` + `EdgeWriteIntent`

`crates/moa-memory/graph/src/node.rs`:

```rust
#[derive(Debug, Clone)]
pub struct NodeWriteIntent {
    pub uid: Uuid,
    pub label: NodeLabel,
    pub workspace_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub scope: String,                  // matches RLS expectations
    pub name: String,
    pub properties: serde_json::Value,  // becomes agtype map
    pub pii_class: PiiClass,
    pub confidence: Option<f64>,
    pub valid_from: DateTime<Utc>,
    pub embedding: Option<Vec<f32>>,    // 1024-dim if present; None means "skip vector"
    pub embedding_model: Option<String>,
    pub embedding_model_version: Option<i32>,
    pub actor_id: Uuid,
    pub actor_kind: String,
}

#[derive(Debug, Clone)]
pub struct EdgeWriteIntent {
    pub uid: Uuid,
    pub label: EdgeLabel,
    pub start_uid: Uuid,
    pub end_uid: Uuid,
    pub properties: serde_json::Value,
    pub workspace_id: Option<Uuid>,
    pub scope: String,
    pub actor_id: Uuid,
    pub actor_kind: String,
}
```

### 5b create_node

```rust
async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid, GraphError> {
    let mut tx = self.pool.begin().await?;
    // 1. AGE Cypher (uses per-label CREATE template)
    let cypher = match intent.label {
        NodeLabel::Fact     => crate::cypher::node::CREATE_FACT,
        NodeLabel::Entity   => crate::cypher::node::CREATE_ENTITY,
        // ... per-label
        _ => return Err(GraphError::Conflict(format!("unsupported create-label {:?}", intent.label))),
    };
    let mut props = intent.properties.clone();
    let m = props.as_object_mut().ok_or(GraphError::Conflict("properties must be object".into()))?;
    m.insert("uid".into(), serde_json::json!(intent.uid.to_string()));
    m.insert("workspace_id".into(), serde_json::json!(intent.workspace_id.map(|w| w.to_string())));
    m.insert("user_id".into(), serde_json::json!(intent.user_id.map(|u| u.to_string())));
    m.insert("scope".into(), serde_json::json!(intent.scope.clone()));
    m.insert("name".into(), serde_json::json!(intent.name.clone()));
    m.insert("pii_class".into(), serde_json::json!(format!("{:?}", intent.pii_class).to_lowercase()));
    m.insert("valid_from".into(), serde_json::json!(intent.valid_from.to_rfc3339()));
    m.insert("created_at".into(), serde_json::json!(Utc::now().to_rfc3339()));
    cypher.execute(&serde_json::json!({"props": props})).execute(&mut *tx).await
        .map_err(|e| GraphError::Cypher(e.to_string()))?;

    // 2. Sidecar UPSERT
    sqlx::query!(
        r#"INSERT INTO moa.node_index
           (uid, label, workspace_id, user_id, name, pii_class, confidence, valid_from)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
        intent.uid, intent.label.as_str(), intent.workspace_id, intent.user_id,
        intent.name, intent.pii_class.as_str(), intent.confidence, intent.valid_from,
    ).execute(&mut *tx).await?;

    // 3. Embedding (pgvector path; Turbopuffer impl runs after commit)
    if let (Some(emb), Some(model), Some(ver)) = (
        intent.embedding.as_ref(), intent.embedding_model.as_ref(), intent.embedding_model_version) {
        // delegated to VectorStore (pgvector impl uses same tx via PgConnection)
        self.vector.upsert_in_tx(&mut tx, &[VectorItem {
            uid: intent.uid, workspace_id: intent.workspace_id, user_id: intent.user_id,
            label: intent.label.as_str().to_string(),
            pii_class: intent.pii_class.as_str().to_string(),
            embedding: emb.clone(),
            embedding_model: model.clone(),
            embedding_model_version: ver, valid_to: None,
        }]).await?;
    }

    // 4. Changelog + workspace_state bump
    crate::changelog::write_and_bump(&mut *tx, ChangelogRecord {
        workspace_id: intent.workspace_id, user_id: intent.user_id,
        scope: intent.scope.clone(), actor_id: Some(intent.actor_id), actor_kind: intent.actor_kind.clone(),
        op: "create".into(), target_kind: "node".into(),
        target_label: intent.label.as_str().to_string(), target_uid: intent.uid,
        payload: serde_json::json!({"after": &intent.properties}),
        pii_class: intent.pii_class.as_str().to_string(),
        audit_metadata: None, cause_change_id: None,
    }).await?;

    tx.commit().await?;
    Ok(intent.uid)
}
```

### 5c supersede_node

```rust
async fn supersede_node(&self, old_uid: Uuid, new: NodeWriteIntent) -> Result<Uuid, GraphError> {
    let mut tx = self.pool.begin().await?;
    let now = Utc::now();
    // Validate old node exists and is not already invalidated.
    let old: Option<NodeIndexRow> = sqlx::query_as!(...).fetch_optional(&mut *tx).await?;
    let old = old.ok_or(GraphError::NotFound(old_uid))?;
    if old.valid_to.is_some() { return Err(GraphError::BiTemporal(format!("{} already invalidated", old_uid))); }
    if new.valid_from <= old.valid_from { return Err(GraphError::BiTemporal("new.valid_from must follow old.valid_from".into())); }

    // 1. AGE supersede via Cypher
    let params = serde_json::json!({
        "old_uid": old_uid.to_string(), "now": now.to_rfc3339(),
        "actor": new.actor_id.to_string(),
        "new_props": props_with_envelope(&new),
    });
    crate::cypher::node::SUPERSEDE.execute(&params).execute(&mut *tx).await
        .map_err(|e| GraphError::Cypher(e.to_string()))?;

    // 2. Sidecar: close old, insert new
    sqlx::query!(
        "UPDATE moa.node_index SET valid_to = $1, invalidated_at = $1, invalidated_by = $2,
         invalidated_reason = 'superseded' WHERE uid = $3",
        now, new.actor_id, old_uid).execute(&mut *tx).await?;
    sqlx::query!("INSERT INTO moa.node_index ...").execute(&mut *tx).await?;

    // 3. Vector: delete old, insert new
    if old.has_embedding { self.vector.delete_in_tx(&mut tx, &[old_uid]).await?; }
    if new.embedding.is_some() { self.vector.upsert_in_tx(&mut tx, &[/*...*/]).await?; }

    // 4. Changelog: two rows (old=update/superseded, new=create) linked via cause_change_id
    let old_change = changelog::write_and_bump(&mut *tx, ChangelogRecord {
        op: "supersede".into(), target_uid: old_uid, /*...*/
    }).await?;
    let _new_change = changelog::write_and_bump(&mut *tx, ChangelogRecord {
        op: "create".into(), target_uid: new.uid, cause_change_id: Some(old_change), /*...*/
    }).await?;

    tx.commit().await?;
    Ok(new.uid)
}
```

### 5d invalidate_node (soft)

```rust
async fn invalidate_node(&self, uid: Uuid, reason: &str) -> Result<(), GraphError> {
    let mut tx = self.pool.begin().await?;
    let now = Utc::now();
    let actor = current_actor(&tx).await?;
    crate::cypher::node::INVALIDATE.execute(&serde_json::json!({
        "uid": uid.to_string(), "now": now.to_rfc3339(), "actor": actor.to_string(), "reason": reason,
    })).execute(&mut *tx).await.map_err(|e| GraphError::Cypher(e.to_string()))?;
    sqlx::query!("UPDATE moa.node_index SET valid_to = $1, invalidated_at = $1,
                  invalidated_by = $2, invalidated_reason = $3 WHERE uid = $4 AND valid_to IS NULL",
                 now, actor, reason, uid).execute(&mut *tx).await?;
    self.vector.delete_in_tx(&mut tx, &[uid]).await?;
    crate::changelog::write_and_bump(&mut *tx, ChangelogRecord {
        op: "invalidate".into(), target_uid: uid, payload: serde_json::json!({"reason": reason}), /*...*/
    }).await?;
    tx.commit().await?;
    Ok(())
}
```

### 5e hard_purge

```rust
async fn hard_purge(&self, uid: Uuid, redaction_marker: &str) -> Result<(), GraphError> {
    let mut tx = self.pool.begin().await?;
    let actor = current_actor(&tx).await?;
    // Read original properties for redacted changelog entry BEFORE deleting
    let row = sqlx::query!("SELECT properties_summary, label, workspace_id, user_id, scope, pii_class
                            FROM moa.node_index WHERE uid = $1", uid)
        .fetch_optional(&mut *tx).await?
        .ok_or(GraphError::NotFound(uid))?;

    crate::cypher::node::HARD_PURGE.execute(&serde_json::json!({"uid": uid.to_string()}))
        .execute(&mut *tx).await.map_err(|e| GraphError::Cypher(e.to_string()))?;
    sqlx::query!("DELETE FROM moa.node_index WHERE uid = $1", uid).execute(&mut *tx).await?;
    self.vector.delete_in_tx(&mut tx, &[uid]).await?;

    // Redacted changelog: replace properties with hash; preserve label/scope for audit
    let payload = serde_json::json!({
        "redaction_marker": redaction_marker,
        "label": row.label,
        "scope": row.scope,
        "properties_hash": blake3::hash(serde_json::to_vec(&row.properties_summary).unwrap().as_slice()).to_hex().to_string(),
    });
    crate::changelog::write_and_bump(&mut *tx, ChangelogRecord {
        op: "erase".into(), target_uid: uid, payload, /*...*/
    }).await?;
    tx.commit().await?;
    Ok(())
}
```

### 5f create_edge — straightforward; one Cypher CREATE + changelog row.

### 5g VectorStore in-tx variants

Add to `VectorStore` trait two more methods:

```rust
async fn upsert_in_tx<'a>(&self, tx: &mut sqlx::Transaction<'a, sqlx::Postgres>, items: &[VectorItem]) -> Result<()>;
async fn delete_in_tx<'a>(&self, tx: &mut sqlx::Transaction<'a, sqlx::Postgres>, uids: &[Uuid]) -> Result<()>;
```

`PgvectorStore` implements them by issuing the same SQL but on the borrowed tx; `TurbopufferStore` (M26) returns an error from these (forces caller to commit Postgres first, then call non-tx variant — different code path).

## 6 Deliverables

- `crates/moa-memory/graph/src/write.rs` (~500 lines).
- Updated `VectorStore` trait + `PgvectorStore` impl with `*_in_tx` variants.
- Round-trip integration test exercising all four modes.

## 7 Acceptance criteria

1. `create_node` followed by `get_node` returns the inserted row and a 1024-dim embedding round-trips.
2. `supersede_node` leaves old `valid_to` set, new node valid; `SUPERSEDES` edge present in AGE; vector deleted then inserted; two changelog rows linked.
3. `invalidate_node` sets `valid_to`, deletes vector, writes one changelog row.
4. `hard_purge` removes from AGE, sidecar, vector; writes a redacted changelog row with payload hash.
5. Failure injected at step 3 of `create_node` rolls back AGE + sidecar (no orphan).
6. `workspace_state.changelog_version` increments by exactly the number of changelog rows committed (1 per create/invalidate/erase, 2 per supersede).

## 8 Tests

```sh
cargo test -p moa-memory-graph write_protocol
cargo test -p moa-memory-graph rollback_on_failure
```

## 9 Cleanup

- Remove all `NotImplemented` stubs added in M07.
- Delete `moa-memory/src/store/file.rs` write paths (read paths stay until M28). Mark each function `#[deprecated]`.
- Remove old wiki "branching" reconciliation code from `moa-memory` — it has no analog here.

## 10 What's next

**M09 — `moa-memory-pii` crate (openai/privacy-filter integration).**
