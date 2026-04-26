# Step M04 — Sidecar projection table `moa.node_index`

_Create the hot-path Postgres table that mirrors AGE node identity and tenancy fields, so retrieval, RLS join planning, NER seed lookup, and PII-class filtering all run on plain SQL with B-tree indexes instead of through agtype operator scans._

## 1 What this step is about

AGE is the canonical store for graph structure (nodes, edges, traversals). But agtype access through `agtype_access_operator` is non-LEAKPROOF, prevents some planner optimizations, and is slow for typed lookups. The sidecar pattern keeps a flat SQL projection of every node's identity and routing fields. The sidecar is updated *in the same transaction* as the AGE write (M08), so it never lags. AGE remains source-of-truth for properties beyond the projected columns.

## 2 Files to read

- `migrations/M03_age_bootstrap.sql` (so the FK target is consistent)
- `crates/moa-core/src/memory/scope.rs` (M01)
- `crates/moa-runtime/src/db/scoped_conn.rs` (M02/M03)

## 3 Goal

A `moa.node_index` table with one row per AGE vertex, FORCE-RLS-protected, indexed for:
- Lookup by `uid` (node identity).
- Lookup by `(workspace_id, scope, label)` for retrieval scoping.
- Lookup by `name` (full-text, for NER seed resolution).
- Filter by `pii_class` and `valid_to IS NULL`.
- Embedding FK target (`uid`) for `moa.embeddings` (created in M05).

## 4 Rules

- **`uid UUID PRIMARY KEY`**: matches the `uid` property inside the AGE node. Never use AGE's internal `id` (`graphid`) as an external reference — it is not preserved across pg_dump/restore.
- **`gid BIGINT NULL`**: optional cache of AGE's `id(node)` for fast bidirectional lookup; populated on insert, refreshed if AGE renumbers (rare).
- **Soft deletion**: rows are not deleted on supersession; instead `valid_to` is set. Hard purge in M24 deletes both AGE node and sidecar row.
- **`label TEXT NOT NULL`**: enum-like; CHECK constraint enforces one of the seven vertex labels.
- **`scope TEXT GENERATED`**: same generated-column pattern as M02 ensures consistency.
- **Full-text on `name`**: tsvector column for NER seed resolution.
- Inherit M02 RLS template verbatim.

## 5 Tasks

### 5a Migration: `migrations/M04_node_index.sql`

```sql
CREATE TABLE moa.node_index (
    uid              UUID PRIMARY KEY,
    gid              BIGINT,
    label            TEXT NOT NULL CHECK (label IN
        ('Entity','Concept','Decision','Incident','Lesson','Fact','Source')),
    workspace_id     UUID,
    user_id          UUID,
    scope            TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    name             TEXT NOT NULL,
    name_tsv         tsvector GENERATED ALWAYS AS (to_tsvector('simple', coalesce(name,''))) STORED,
    pii_class        TEXT NOT NULL DEFAULT 'none' CHECK (pii_class IN ('none','pii','phi','restricted')),
    confidence       DOUBLE PRECISION,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_from       TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to         TIMESTAMPTZ,
    invalidated_at   TIMESTAMPTZ,
    invalidated_by   UUID,
    invalidated_reason TEXT,
    last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    properties_summary JSONB
);

-- Hot-path indexes
CREATE INDEX node_index_uid_idx        ON moa.node_index (uid);
CREATE INDEX node_index_ws_scope_label ON moa.node_index (workspace_id, scope, label) WHERE valid_to IS NULL;
CREATE INDEX node_index_name_tsv_idx   ON moa.node_index USING GIN (name_tsv);
CREATE INDEX node_index_pii_idx        ON moa.node_index (pii_class) WHERE valid_to IS NULL;
CREATE INDEX node_index_validto_partial_idx ON moa.node_index (valid_to) WHERE valid_to IS NULL;
CREATE INDEX node_index_label_partial  ON moa.node_index (label) WHERE valid_to IS NULL;
CREATE INDEX node_index_lastaccess_idx ON moa.node_index (last_accessed_at);

-- RLS
ALTER TABLE moa.node_index ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.node_index FORCE ROW LEVEL SECURITY;

CREATE POLICY rd_global ON moa.node_index FOR SELECT TO moa_app USING (scope = 'global');
CREATE POLICY rd_workspace ON moa.node_index FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY rd_user ON moa.node_index FOR SELECT TO moa_app
  USING (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());

CREATE POLICY wr_workspace ON moa.node_index FOR ALL TO moa_app
  USING      (scope = 'workspace' AND workspace_id = moa.current_workspace())
  WITH CHECK (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY wr_user ON moa.node_index FOR ALL TO moa_app
  USING      (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id())
  WITH CHECK (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());
CREATE POLICY wr_global ON moa.node_index FOR ALL TO moa_promoter
  USING (scope = 'global') WITH CHECK (scope = 'global');

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.node_index TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.node_index TO moa_promoter;
```

### 5b Rust types

`crates/moa-memory-graph/src/node.rs` (the crate is scaffolded properly in M07; for now create a minimal stub):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIndexRow {
    pub uid: Uuid,
    pub label: NodeLabel,
    pub workspace_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub scope: String,
    pub name: String,
    pub pii_class: PiiClass,
    pub valid_to: Option<DateTime<Utc>>,
    pub last_accessed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text")]
#[serde(rename_all = "PascalCase")]
pub enum NodeLabel {
    Entity, Concept, Decision, Incident, Lesson, Fact, Source,
}
```

### 5c NER seed-lookup helper

```rust
pub async fn lookup_seed_by_name(
    conn: &mut sqlx::PgConnection, name: &str, limit: i64,
) -> Result<Vec<NodeIndexRow>> {
    sqlx::query_as!(
        NodeIndexRow,
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, last_accessed_at
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND name_tsv @@ plainto_tsquery('simple', $1)
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                 last_accessed_at DESC
        LIMIT $2
        "#,
        name, limit
    ).fetch_all(&mut *conn).await.map_err(Into::into)
}
```

(RLS scopes the result automatically.)

### 5d Bump-on-read helper

```rust
pub async fn bump_last_accessed(conn: &mut sqlx::PgConnection, uids: &[Uuid]) -> Result<()> {
    sqlx::query!(
        "UPDATE moa.node_index SET last_accessed_at = now() WHERE uid = ANY($1)",
        uids,
    ).execute(&mut *conn).await?;
    Ok(())
}
```

## 6 Deliverables

- `migrations/M04_node_index.sql` (~120 lines).
- `crates/moa-memory-graph/src/node.rs` minimal stub (~80 lines).
- `crates/moa-memory-graph/Cargo.toml` (new crate; full scaffold in M07; for now: lib, sqlx, uuid, chrono, serde, serde_json deps).
- Add `moa-memory-graph` to workspace members.

## 7 Acceptance criteria

1. Migration applies cleanly.
2. Insert 100 rows across 3 workspaces; cross-tenant SELECT returns only own rows.
3. `EXPLAIN SELECT * FROM moa.node_index WHERE workspace_id = $1 AND scope = 'workspace' AND label = 'Fact' AND valid_to IS NULL` shows index scan on `node_index_ws_scope_label`.
4. NER seed lookup test: 1000 rows, query "auth service" hits via tsvector; latency <2ms warm.

## 8 Tests

```sh
cargo run --bin migrate
cargo test -p moa-memory-graph node_index
psql -d moa -c "EXPLAIN ANALYZE SELECT * FROM moa.node_index WHERE name_tsv @@ plainto_tsquery('simple', 'auth')"
```

## 9 Cleanup

- Confirm no leftover `MEMORY.md`-style index tables exist in any pre-existing migration. If a pre-3-tier `moa.wiki_index` or similar exists, drop it in this migration.

## 10 What's next

**M05 — `VectorStore` trait + pgvector impl with halfvec(1024).**
