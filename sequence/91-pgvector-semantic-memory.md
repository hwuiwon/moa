# Step 91 — pgvector Semantic Memory Search + Hybrid Retrieval

_tsvector (step 90) finds pages by keyword. pgvector finds pages by meaning. Run both in parallel, fuse the rankings with Reciprocal Rank Fusion, and the brain gets measurably better retrieval on under-specified queries. Embeddings are computed at page-write time, stored in the same `wiki_pages` table, indexed with HNSW._

---

## 1. What this step is about

Keyword search fails on "the thing we discussed last week about token stuff" — no exact word matches pages on authentication. Semantic search fails on precise technical identifiers — embeddings conflate `jwt` and `oauth2_token` that a keyword index keeps distinct. Both fail differently, and fusing them is better than either alone.

This step adds a second retrieval path:

1. When a wiki page is written, compute a text embedding of `{title}\n\n{content}` using a small, cheap embedding model.
2. Store the vector in a new `embedding vector(1536)` column on `wiki_pages`.
3. On search, run both tsvector and pgvector queries in parallel. Fuse the two ranked lists with Reciprocal Rank Fusion (RRF), default k=60.
4. Return the top N by fused rank.

This is the classic "hybrid search" pattern. BEIR benchmarks typically show hybrid beats either component by 5–15% nDCG on mixed workloads. For an agent that doesn't know in advance whether a user's query is keyword-precise or vaguely-worded, hybrid is the safer default.

---

## 2. Files to read

- `moa-memory/src/search.rs` — step 90's `WikiSearchIndex`. Extends here.
- `moa-memory/migrations/001_wiki_pages.sql` — the schema to extend.
- `moa-memory/src/wiki.rs` — `WikiPage` definition.
- `moa-providers/src/*.rs` — for embedding model calls. Anthropic has no embeddings API; OpenAI's `text-embedding-3-small` (1536-dim, $0.02/MTok) is the default. Gemini's `text-embedding-004` (768-dim) is an alternative.
- pgvector docs: HNSW index tuning, operator `<=>` (cosine distance), `<#>` (inner product), `<->` (L2).
- Neon docs: pgvector is enabled by default; no extra configuration.

---

## 3. Goal

1. `wiki_pages` gets an `embedding vector(1536)` column and an HNSW index.
2. `WikiSearchIndex::upsert_page` asynchronously computes the embedding and persists it. If the embedding call fails, the page is written without embedding (tsvector still works); a background task retries.
3. `WikiSearchIndex::search` runs tsvector and pgvector in parallel, fuses with RRF, returns a single ranked list.
4. An `EmbeddingProvider` trait abstracts the model choice. Default: OpenAI `text-embedding-3-small`. Configurable per installation.
5. A `moa memory rebuild-embeddings` CLI subcommand backfills embeddings for existing pages (one-time migration after this step lands).
6. The agent's `memory_search` tool uses hybrid search by default; callers can opt out with `mode: "keyword"` or `mode: "semantic"` for specific needs.

---

## 4. Rules

- **Embeddings are eventually-consistent.** Writing a markdown file returns success as soon as the file is on disk and the row (with or without embedding) is in Postgres. The embedding call can fail or lag without blocking the user.
- **Batch embedding calls.** OpenAI allows up to 2048 inputs per request. A background worker collects pages needing embeddings and batches them.
- **Vector column is `NOT NULL` deferred.** Define the column as nullable; treat NULL as "not yet embedded." Backfill on demand.
- **HNSW over IVFFlat.** HNSW builds slower but queries faster and does not require training; IVFFlat requires an `ANALYZE` pass per workspace. For agent retrieval, HNSW is the right pick.
- **One embedding model per installation.** Switching models invalidates all vectors. If config changes the embedding model, `moa doctor` warns loudly and suggests `moa memory rebuild-embeddings`.
- **RRF with k=60.** Well-established default. Don't invent a custom fusion scheme; RRF is standard and works.
- **Cost budget.** At `text-embedding-3-small` ($0.02/MTok), embedding 10K pages averaging 2K tokens each costs $0.40. Not a cost concern.

---

## 5. Tasks

### 5a. Schema extension

Add a second migration `moa-memory/migrations/002_wiki_embeddings.sql`:

```sql
CREATE EXTENSION IF NOT EXISTS vector;

ALTER TABLE wiki_pages
    ADD COLUMN embedding vector(1536),
    ADD COLUMN embedding_model TEXT,
    ADD COLUMN embedding_updated TIMESTAMPTZ;

-- HNSW index for cosine distance
CREATE INDEX wiki_pages_embedding_hnsw
    ON wiki_pages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

-- Queue of pages needing embedding (or re-embedding after content update)
CREATE TABLE wiki_embedding_queue (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (scope, path)
);

CREATE INDEX wiki_embedding_queue_enqueued ON wiki_embedding_queue (enqueued_at);
```

### 5b. `EmbeddingProvider` trait

```rust
// moa-providers/src/embedding.rs
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn model_id(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>>;
}

pub struct OpenAIEmbedding {
    client: reqwest::Client,
    api_key: SecretString,
    model: String, // "text-embedding-3-small"
}

pub struct GeminiEmbedding { /* 768-dim alternative */ }

pub struct MockEmbedding {
    // Deterministic hash-based vectors for tests; no network call.
    dimensions: usize,
}
```

Tests use `MockEmbedding`. Production uses `OpenAIEmbedding`.

### 5c. Enqueue on write, process in background

`WikiSearchIndex::upsert_page` enqueues into `wiki_embedding_queue`:

```rust
pub async fn upsert_page(&self, scope: &MemoryScope, path: &MemoryPath, page: &WikiPage) -> Result<()> {
    let mut tx = self.pool.begin().await.map_err(memory_error)?;

    // upsert wiki_pages row (step 90 logic)
    sqlx::query(r#"INSERT INTO wiki_pages ... ON CONFLICT ... DO UPDATE ..."#)
        // ... bindings ...
        .execute(&mut *tx).await.map_err(memory_error)?;

    // Enqueue for embedding (even if one exists — content may have changed)
    sqlx::query(r#"
        INSERT INTO wiki_embedding_queue (scope, path) VALUES ($1, $2)
        ON CONFLICT (scope, path) DO UPDATE SET enqueued_at = NOW(), attempt_count = 0, last_error = NULL
    "#)
    .bind(scope_key(scope))
    .bind(path.as_str())
    .execute(&mut *tx).await.map_err(memory_error)?;

    tx.commit().await.map_err(memory_error)?;
    Ok(())
}
```

A background task drains the queue:

```rust
pub async fn embedding_worker(
    pool: Arc<PgPool>,
    embedder: Arc<dyn EmbeddingProvider>,
) {
    let batch_size = 64;
    loop {
        let claimed = sqlx::query(r#"
            WITH claimed AS (
                SELECT scope, path FROM wiki_embedding_queue
                WHERE attempt_count < 5
                ORDER BY enqueued_at
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            SELECT q.scope, q.path, p.title, p.content
            FROM claimed q
            JOIN wiki_pages p ON p.scope = q.scope AND p.path = q.path
        "#)
        .bind(batch_size as i64)
        .fetch_all(&*pool).await;

        match claimed {
            Ok(rows) if !rows.is_empty() => {
                let inputs: Vec<String> = rows.iter().map(|r| {
                    format!("{}\n\n{}", r.get::<String, _>("title"), r.get::<String, _>("content"))
                }).collect();

                match embedder.embed(&inputs).await {
                    Ok(vectors) => persist_embeddings(&pool, &rows, &vectors, embedder.model_id()).await,
                    Err(e) => mark_failures(&pool, &rows, &e.to_string()).await,
                }
            }
            Ok(_) => tokio::time::sleep(Duration::from_secs(5)).await, // queue empty
            Err(e) => {
                tracing::warn!(error=%e, "embedding queue scan failed");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}
```

`FOR UPDATE SKIP LOCKED` makes this safe to run on multiple processes simultaneously — each one claims a disjoint batch.

### 5d. Hybrid search query

Replace `WikiSearchIndex::search` with:

```rust
pub async fn search(&self, query: &str, scope: &MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>> {
    let (tsv_results, vec_results) = tokio::try_join!(
        self.search_tsvector(query, scope, limit * 2),     // fetch 2× for fusion
        self.search_semantic(query, scope, limit * 2),
    )?;

    let fused = reciprocal_rank_fusion(&tsv_results, &vec_results, 60);
    Ok(fused.into_iter().take(limit).collect())
}

async fn search_semantic(&self, query: &str, scope: &MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>> {
    let query_embedding: Vec<f32> = self.embedder.embed(&[query.to_string()]).await?.remove(0);

    let sql = r#"
        SELECT
            path, title, page_type, confidence, updated, reference_count, content,
            1 - (embedding <=> $1::vector) AS similarity
        FROM wiki_pages
        WHERE scope = $2 AND embedding IS NOT NULL
        ORDER BY embedding <=> $1::vector
        LIMIT $3
    "#;

    let rows = sqlx::query(sql)
        .bind(PgVector::from(query_embedding))
        .bind(scope_key(scope))
        .bind(limit as i64)
        .fetch_all(&*self.pool).await.map_err(memory_error)?;

    rows.into_iter().map(|r| -> Result<_> {
        Ok(MemorySearchResult {
            scope: scope.clone(),
            path: MemoryPath::new(r.get::<String, _>("path")),
            title: r.get("title"),
            page_type: parse_page_type(r.get("page_type"))?,
            snippet: extract_snippet_around_keywords(r.get::<String, _>("content").as_str(), query),
            confidence: parse_confidence(r.get("confidence"))?,
            updated: r.get("updated"),
            reference_count: r.get::<i32, _>("reference_count") as u64,
        })
    }).collect()
}

fn reciprocal_rank_fusion(
    a: &[MemorySearchResult],
    b: &[MemorySearchResult],
    k: u32,
) -> Vec<MemorySearchResult> {
    let mut scores: HashMap<(MemoryScope, MemoryPath), (f64, MemorySearchResult)> = HashMap::new();

    for (rank, result) in a.iter().enumerate() {
        let key = (result.scope.clone(), result.path.clone());
        let score = 1.0 / (k as f64 + rank as f64 + 1.0);
        scores.entry(key).or_insert_with(|| (0.0, result.clone())).0 += score;
    }
    for (rank, result) in b.iter().enumerate() {
        let key = (result.scope.clone(), result.path.clone());
        let score = 1.0 / (k as f64 + rank as f64 + 1.0);
        scores.entry(key).or_insert_with(|| (0.0, result.clone())).0 += score;
    }

    let mut fused: Vec<_> = scores.into_values().collect();
    fused.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    fused.into_iter().map(|(_, r)| r).collect()
}
```

For pages without embeddings (queue not yet drained), they appear only in the tsvector list. That's a reasonable degradation.

### 5e. `memory_search` tool extension

The agent-facing tool schema gets a `mode` parameter:

```json
{
  "name": "memory_search",
  "parameters": {
    "query": "string",
    "scope": "enum(user, workspace, both)",
    "limit": "integer, default 5",
    "mode": "enum(hybrid, keyword, semantic) default=hybrid"
  }
}
```

Default is hybrid. The brain rarely needs to force a mode; expose it for transparency and for tool-call tests.

### 5f. CLI: `moa memory rebuild-embeddings`

```rust
pub async fn cmd_rebuild_embeddings(ctx: &Context, scope: Option<MemoryScope>) -> Result<()> {
    let pool = ctx.pool();
    // Clear current embeddings and re-enqueue everything
    let rows_affected = sqlx::query(r#"
        WITH enq AS (
            INSERT INTO wiki_embedding_queue (scope, path)
            SELECT scope, path FROM wiki_pages
            WHERE ($1::text IS NULL OR scope = $1)
            ON CONFLICT (scope, path) DO UPDATE SET enqueued_at = NOW(), attempt_count = 0, last_error = NULL
            RETURNING 1
        )
        SELECT COUNT(*) FROM enq
    "#)
    .bind(scope.as_ref().map(scope_key))
    .fetch_one(pool).await?;

    println!("enqueued {rows_affected} pages for re-embedding; worker will process in background");
    Ok(())
}
```

### 5g. `moa doctor` additions

Check:
- pgvector extension version.
- Count of pages with `embedding IS NULL` — report as "backfill needed."
- Queue depth on `wiki_embedding_queue` — report if growing.
- Cardinality of `embedding_model` values in `wiki_pages` — if mixed, flag mismatch and suggest rebuild.

### 5h. Observability

Metrics (via the existing metrics pipeline):
- `moa_embedding_queue_depth` — gauge.
- `moa_embeddings_computed_total` — counter.
- `moa_embedding_failures_total` — counter by error type.
- `moa_search_hybrid_latency_seconds` — histogram, with label `component=tsvector|semantic|fusion`.

### 5i. Tests

- Unit: `reciprocal_rank_fusion` fuses two lists correctly, handles overlap, preserves order by fused score.
- Integration: write 20 pages, let the worker drain, run hybrid search; assert top result on a semantic query comes from the vector side; assert top result on an exact-match query comes from the tsvector side.
- Integration: simulate embedding provider failure, assert the page is still searchable (via tsvector) and the queue retries up to 5 times.
- Integration: `memory_search` tool call with `mode: "semantic"` bypasses the tsvector path.
- Integration: switching `embedding_model` config, running `moa doctor`, getting a mismatch warning.

### 5j. Documentation

Update `moa/docs/04-memory-architecture.md` again:

```
Search paths:
1. Keyword: tsvector + GIN (step 90). Exact-term matches, phrase search, typo-tolerant fallback.
2. Semantic: pgvector + HNSW (step 91). Meaning-based retrieval.
3. Hybrid (default): run both in parallel, fuse with RRF.

Embeddings are computed asynchronously. New pages are searchable by keyword
immediately; semantic ranking lags by seconds to minutes depending on queue depth.
```

---

## 6. Deliverables

- [ ] Migration `002_wiki_embeddings.sql` with `vector(1536)` column, HNSW index, queue table.
- [ ] `EmbeddingProvider` trait + OpenAI and Mock implementations.
- [ ] Embedding worker task running under the orchestrator (or as a separate binary).
- [ ] `WikiSearchIndex::search_semantic` and `reciprocal_rank_fusion`.
- [ ] `memory_search` tool schema extended with `mode` parameter.
- [ ] `moa memory rebuild-embeddings` CLI.
- [ ] `moa doctor` reports pgvector version, queue depth, model mismatch.
- [ ] Metrics for queue depth, embedding success/failure, search latency by component.
- [ ] Tests covering fusion logic, hybrid search, queue retry, mode override, model mismatch.
- [ ] `moa/docs/04-memory-architecture.md` updated with the hybrid retrieval story.

---

## 7. Acceptance criteria

1. Hybrid search on "the token refresh thing we discussed" returns the OAuth page in the top 3 at 1K pages.
2. Hybrid search on an exact technical term (e.g., a unique function name appearing in one page) returns that page as rank 1.
3. An unembedded page (queue not yet drained) still appears in results via tsvector — graceful degradation works.
4. The embedding worker, running as a single process, drains a queue of 10K pages in under 60 seconds (OpenAI batched API, network-bound).
5. Two worker processes run concurrently and neither double-embeds the same page — `FOR UPDATE SKIP LOCKED` verified.
6. `EXPLAIN ANALYZE` of the hybrid search shows both `Bitmap Index Scan on wiki_pages_tsv_gin` and `Index Scan using wiki_pages_embedding_hnsw`.
7. `moa memory rebuild-embeddings` re-enqueues every page and the worker repopulates vectors within the expected time window.
8. Hybrid search p95 latency at 10K pages stays under 100ms (embed call + both queries + fusion).
9. Setting `mode: "keyword"` in a `memory_search` tool call bypasses the embedding call entirely — verified by asserting `moa_embeddings_computed_total` doesn't tick.
10. Configuring a different embedding model in config produces a `moa doctor` warning that explicitly suggests `moa memory rebuild-embeddings`.
