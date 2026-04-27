# Step M05 — `VectorStore` trait + pgvector impl with halfvec(1024)

_Define the abstract `VectorStore` trait that both pgvector (default) and Turbopuffer (M26) implement, and ship the pgvector implementation backed by `halfvec(1024)` with HNSW, hash-partitioned by workspace, and FORCE-RLS scoped._

## 1 What this step is about

Cohere Embed v4 emits 1024-dim float32 vectors. We store them as `halfvec(1024)` (16-bit per dim) for ~57% storage reduction and faster HNSW build. Hash-partitioning by `workspace_id` (32 partitions) keeps each partition's HNSW index small and lets the planner prune to one partition per query. The trait abstracts over backend so M26 can plug in Turbopuffer without touching consumers.

## 2 Files to read

- M00 stack-pin (pgvector 0.8.2)
- M02 RLS template
- M04 `moa.node_index` (vector FK target)
- Cohere Embed v4 docs (1024-dim Matryoshka)

## 3 Goal

1. New crate `moa-memory-vector` with the `VectorStore` trait.
2. `PgvectorStore` impl using sqlx + pgvector's `halfvec` Rust binding.
3. Migration `M05_embeddings.sql` creates the partitioned `moa.embeddings` table with HNSW index, FORCE-RLS, and FK to `moa.node_index`.
4. Cohere Embed v4 client integrated as the default embedder (behind an `Embedder` trait so we can swap models).

## 4 Rules

- **Dimension 1024**, halfvec representation.
- **`halfvec_cosine_ops`** for the HNSW operator class. Always query with `<=>` to match.
- **32 hash partitions** named `moa.embeddings_p00` … `moa.embeddings_p31`. Tunable but fixed by migration; resharding is a separate operation.
- **FK to `moa.node_index(uid)`** with `ON DELETE CASCADE`. Hard-purge of a node automatically deletes its embedding.
- **`embedding_model_version INT NOT NULL`** column for safe model upgrades (M26 dual-write pattern).
- **Trait is async** (`#[async_trait]`).

## 5 Tasks

### 5a Migration: `migrations/M05_embeddings.sql`

```sql
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE moa.embeddings (
    uid                     UUID NOT NULL,
    workspace_id            UUID,
    user_id                 UUID,
    scope                   TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    label                   TEXT NOT NULL,
    pii_class               TEXT NOT NULL DEFAULT 'none',
    embedding               halfvec(1024) NOT NULL,
    embedding_model         TEXT NOT NULL,
    embedding_model_version INT  NOT NULL,
    valid_to                TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, uid)
) PARTITION BY HASH (workspace_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE moa.embeddings_p%s PARTITION OF moa.embeddings FOR VALUES WITH (MODULUS 32, REMAINDER %s)',
            lpad(i::text, 2, '0'), i);
    END LOOP;
END $$;

CREATE INDEX ON moa.embeddings USING hnsw (embedding halfvec_cosine_ops);
CREATE INDEX embeddings_ws_scope_label_idx ON moa.embeddings (workspace_id, scope, label) WHERE valid_to IS NULL;
CREATE INDEX embeddings_uid_idx ON moa.embeddings (uid);

ALTER TABLE moa.embeddings ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.embeddings FORCE ROW LEVEL SECURITY;

-- 3-tier RLS template (same as M02/M04)
CREATE POLICY rd_global ON moa.embeddings FOR SELECT TO moa_app USING (scope = 'global');
CREATE POLICY rd_workspace ON moa.embeddings FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY rd_user ON moa.embeddings FOR SELECT TO moa_app
  USING (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());
CREATE POLICY wr_workspace ON moa.embeddings FOR ALL TO moa_app
  USING      (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY wr_global ON moa.embeddings FOR ALL TO moa_promoter
  USING (scope = 'global') WITH CHECK (scope = 'global');

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.embeddings TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.embeddings TO moa_promoter;
```

### 5b `VectorStore` trait

`crates/moa-memory-vector/src/lib.rs`:

```rust
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct VectorItem {
    pub uid: Uuid,
    pub workspace_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub label: String,
    pub pii_class: String,
    pub embedding: Vec<f32>,                // 1024-dim
    pub embedding_model: String,
    pub embedding_model_version: i32,
    pub valid_to: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
pub struct VectorQuery {
    pub embedding: Vec<f32>,
    pub k: usize,
    pub label_filter: Option<Vec<String>>,
    pub max_pii_class: String,              // hierarchical: none<pii<phi<restricted
    pub include_global: bool,
}

#[derive(Debug, Clone)]
pub struct VectorMatch {
    pub uid: Uuid,
    pub score: f32,                         // cosine similarity (1 = identical, -1 = opposite)
}

#[async_trait]
pub trait VectorStore: Send + Sync {
    fn backend(&self) -> &'static str;
    fn dimension(&self) -> usize;
    async fn upsert(&self, items: &[VectorItem]) -> anyhow::Result<()>;
    async fn knn(&self, query: &VectorQuery) -> anyhow::Result<Vec<VectorMatch>>;
    async fn delete(&self, uids: &[Uuid]) -> anyhow::Result<()>;
}
```

### 5c `PgvectorStore` impl

`crates/moa-memory-vector/src/pgvector_store.rs`:

```rust
use sqlx::PgPool;
use pgvector::HalfVector;

pub struct PgvectorStore { pool: PgPool }

impl PgvectorStore {
    pub fn new(pool: PgPool) -> Self { Self { pool } }
}

#[async_trait::async_trait]
impl VectorStore for PgvectorStore {
    fn backend(&self) -> &'static str { "pgvector" }
    fn dimension(&self) -> usize { 1024 }

    async fn upsert(&self, items: &[VectorItem]) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for it in items {
            assert_eq!(it.embedding.len(), 1024);
            let hv = HalfVector::from(it.embedding.clone());
            sqlx::query(
                r#"INSERT INTO moa.embeddings
                   (uid, workspace_id, user_id, label, pii_class, embedding, embedding_model, embedding_model_version, valid_to)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                   ON CONFLICT (workspace_id, uid) DO UPDATE
                     SET embedding = EXCLUDED.embedding,
                         embedding_model = EXCLUDED.embedding_model,
                         embedding_model_version = EXCLUDED.embedding_model_version,
                         valid_to = EXCLUDED.valid_to"#,
            )
            .bind(it.uid).bind(it.workspace_id).bind(it.user_id)
            .bind(&it.label).bind(&it.pii_class)
            .bind(hv).bind(&it.embedding_model).bind(it.embedding_model_version).bind(it.valid_to)
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn knn(&self, q: &VectorQuery) -> anyhow::Result<Vec<VectorMatch>> {
        let hv = HalfVector::from(q.embedding.clone());
        // Note: <=> is cosine distance; score = 1 - distance
        let rows = sqlx::query_as::<_, (Uuid, f32)>(
            r#"SELECT uid, 1.0 - (embedding <=> $1)::float4 AS score
               FROM moa.embeddings
               WHERE valid_to IS NULL
                 AND ($2::text[] IS NULL OR label = ANY($2))
               ORDER BY embedding <=> $1
               LIMIT $3"#,
        )
        .bind(hv)
        .bind(q.label_filter.as_deref())
        .bind(q.k as i64)
        .fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(uid, score)| VectorMatch { uid, score }).collect())
    }

    async fn delete(&self, uids: &[Uuid]) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM moa.embeddings WHERE uid = ANY($1)")
            .bind(uids).execute(&self.pool).await?;
        Ok(())
    }
}
```

### 5d Embedder trait + Cohere v4 impl

`crates/moa-memory-vector/src/embedder.rs`:

```rust
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    fn model_name(&self) -> &'static str;
    fn model_version(&self) -> i32;
    fn dimension(&self) -> usize;
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

pub struct CohereV4Embedder {
    client: reqwest::Client,
    api_key: secrecy::SecretString,
}

#[async_trait::async_trait]
impl Embedder for CohereV4Embedder {
    fn model_name(&self) -> &'static str { "cohere-embed-v4" }
    fn model_version(&self) -> i32 { 1 }
    fn dimension(&self) -> usize { 1024 }
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        // POST https://api.cohere.com/v2/embed
        // body: { model: "embed-v4.0", texts, input_type: "search_document",
        //         embedding_types: ["float"], output_dimension: 1024 }
        // truncated for brevity — full impl ~80 lines with retries
        unimplemented!("Cohere v4 embed call")
    }
}
```

### 5e Add `moa-memory-vector` to workspace; wire deps

```toml
# crates/moa-memory-vector/Cargo.toml
[package]
name = "moa-memory-vector"
version.workspace = true
edition.workspace = true

[dependencies]
async-trait = "0.1"
sqlx = { workspace = true, features = ["postgres","runtime-tokio-rustls","uuid","chrono","macros"] }
pgvector = { version = "0.4", features = ["sqlx", "halfvec"] }
uuid = { workspace = true }
chrono = { workspace = true }
serde = { workspace = true }
anyhow = "1"
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
secrecy = "0.10"
```

## 6 Deliverables

- `migrations/M05_embeddings.sql` (~140 lines).
- `crates/moa-memory-vector/src/lib.rs` — trait (~80 lines).
- `crates/moa-memory-vector/src/pgvector_store.rs` (~150 lines).
- `crates/moa-memory-vector/src/embedder.rs` (~120 lines).
- Cargo.toml updates.

## 7 Acceptance criteria

1. Migration applies; 32 partitions exist; HNSW index exists.
2. Round-trip test: insert 100 vectors, KNN top-10, expected nearest is the seed.
3. Cross-tenant KNN test: vector from workspace A invisible to workspace B query.
4. P95 latency <15ms for KNN k=10 on 100k-vector dataset (warm cache).
5. `cargo doc -p moa-memory-vector` clean.

## 8 Tests

```sh
cargo run --bin migrate
cargo test -p moa-memory-vector pgvector_round_trip
cargo test -p moa-memory-vector cross_tenant_knn
cargo bench -p moa-memory-vector knn_p95   # optional, gated on bench feature
```

## 9 Cleanup

- **Delete any pre-existing pgvector schema** that used full-precision `vector` (not `halfvec`) or different dimensionality. The migration drops them via `DROP TABLE IF EXISTS moa.embeddings_old`.
- **Remove old embedder code** from `moa-memory` crate (any `embed.rs` or `vector.rs` modules) — they'll be cleanly excised in M13. Mark with `#[deprecated]` for now.

## 10 What's next

**M06 — `graph_changelog` outbox + Debezium configuration**.
