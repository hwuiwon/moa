//! Postgres-backed wiki search with keyword, semantic, and hybrid retrieval.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use moa_core::{
    ConfidenceLevel, MemoryPath, MemoryScope, MemorySearchMode, MemorySearchResult, MoaError,
    PageType, Result, WikiPage, record_embedding_queue_depth,
};
use moa_providers::EmbeddingProvider;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, ObservableGauge};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use tokio::time::sleep;

use crate::{memory_error, schema};

const SEARCH_DISABLED_MESSAGE: &str =
    "wiki search requires the Postgres tsvector index — see step 90";
const REBUILD_BATCH_SIZE: usize = 256;
const EMBEDDING_BATCH_SIZE: i64 = 64;
const EMBEDDING_IDLE_DELAY: Duration = Duration::from_secs(5);
const EMBEDDING_ERROR_DELAY: Duration = Duration::from_secs(10);
const HYBRID_RRF_K: u32 = 60;
const HYBRID_FETCH_MULTIPLIER: usize = 2;

struct WikiSearchBackend {
    pool: Arc<PgPool>,
    table_name: String,
    queue_table_name: String,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    worker_started: AtomicBool,
}

/// Snapshot of embedding-index health for doctor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingIndexStatus {
    /// Currently configured embedding model when semantic search is enabled.
    pub configured_model: Option<String>,
    /// Number of indexed pages missing an embedding vector.
    pub missing_embeddings: u64,
    /// Number of queued pages awaiting embedding work.
    pub queue_depth: u64,
    /// Number of pages embedded with a model that differs from the configured model.
    pub mismatched_model_pages: u64,
    /// Distinct embedding model ids stored in the table.
    pub stored_models: Vec<String>,
}

/// Search index facade for wiki pages.
#[derive(Clone, Default)]
pub struct WikiSearchIndex {
    backend: Option<Arc<WikiSearchBackend>>,
}

impl WikiSearchIndex {
    /// Creates a disabled wiki search index handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a Postgres-backed wiki search index handle and runs migrations.
    pub async fn new_with_pool(pool: Arc<PgPool>, schema_name: Option<&str>) -> Result<Self> {
        Self::new_with_backend(pool, schema_name, None).await
    }

    /// Creates a Postgres-backed wiki search index handle with semantic search enabled.
    pub async fn new_with_pool_and_embedder(
        pool: Arc<PgPool>,
        schema_name: Option<&str>,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Result<Self> {
        Self::new_with_backend(pool, schema_name, Some(embedder)).await
    }

    /// Starts the background embedding worker when semantic search is enabled.
    pub fn start_embedding_worker(&self) {
        let Some(backend) = &self.backend else {
            return;
        };
        if backend.embedder.is_none() {
            return;
        }
        if backend
            .worker_started
            .compare_exchange(
                false,
                true,
                AtomicOrdering::Relaxed,
                AtomicOrdering::Relaxed,
            )
            .is_ok()
        {
            spawn_embedding_worker(backend.clone());
        }
    }

    /// Searches wiki content within a memory scope using the default hybrid mode.
    pub async fn search(
        &self,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        self.search_with_mode(query, scope, limit, MemorySearchMode::Hybrid)
            .await
    }

    /// Searches wiki content within a memory scope using an explicit retrieval mode.
    pub async fn search_with_mode(
        &self,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
        mode: MemorySearchMode,
    ) -> Result<Vec<MemorySearchResult>> {
        let q = query.trim();
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let backend = self.backend()?;
        match mode {
            MemorySearchMode::Keyword => self.search_keyword(backend, q, scope, limit).await,
            MemorySearchMode::Semantic => self.search_semantic(backend, q, scope, limit).await,
            MemorySearchMode::Hybrid => self.search_hybrid(backend, q, scope, limit).await,
        }
    }

    /// Upserts one wiki page into the search index and queues it for re-embedding.
    pub async fn upsert_page(
        &self,
        scope: &MemoryScope,
        path: &MemoryPath,
        page: &WikiPage,
    ) -> Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let reference_count = reference_count_to_i32(page.reference_count)?;
        let mut tx = backend.pool.begin().await.map_err(memory_error)?;

        sqlx::query(&upsert_sql(&backend.table_name))
            .bind(scope_key(scope))
            .bind(path.as_str())
            .bind(&page.title)
            .bind(page_type_as_str(&page.page_type))
            .bind(confidence_as_str(&page.confidence))
            .bind(page.created)
            .bind(page.updated)
            .bind(page.last_referenced)
            .bind(reference_count)
            .bind(&page.tags)
            .bind(&page.content)
            .execute(&mut *tx)
            .await
            .map_err(memory_error)?;

        if backend.embedder.is_some() {
            sqlx::query(&enqueue_page_sql(&backend.queue_table_name))
                .bind(scope_key(scope))
                .bind(path.as_str())
                .execute(&mut *tx)
                .await
                .map_err(memory_error)?;
        }

        tx.commit().await.map_err(memory_error)?;
        refresh_queue_depth(backend).await?;
        Ok(())
    }

    /// Removes one wiki page from the search index and embedding queue.
    pub async fn delete_page(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let scope_key = scope_key(scope);
        let mut tx = backend.pool.begin().await.map_err(memory_error)?;

        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1 AND path = $2",
            backend.queue_table_name
        ))
        .bind(&scope_key)
        .bind(path.as_str())
        .execute(&mut *tx)
        .await
        .map_err(memory_error)?;

        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1 AND path = $2",
            backend.table_name
        ))
        .bind(&scope_key)
        .bind(path.as_str())
        .execute(&mut *tx)
        .await
        .map_err(memory_error)?;

        tx.commit().await.map_err(memory_error)?;
        refresh_queue_depth(backend).await?;
        Ok(())
    }

    /// Rebuilds the search index for one scope from file-backed wiki pages.
    pub async fn rebuild_scope(
        &self,
        scope: &MemoryScope,
        pages: &[(MemoryPath, WikiPage)],
    ) -> Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let scope_key = scope_key(scope);
        let mut tx = backend.pool.begin().await.map_err(memory_error)?;

        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1",
            backend.queue_table_name
        ))
        .bind(&scope_key)
        .execute(&mut *tx)
        .await
        .map_err(memory_error)?;

        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1",
            backend.table_name
        ))
        .bind(&scope_key)
        .execute(&mut *tx)
        .await
        .map_err(memory_error)?;

        for batch in pages.chunks(REBUILD_BATCH_SIZE) {
            let prepared_batch = batch
                .iter()
                .map(|(path, page)| {
                    Ok::<_, MoaError>((path, page, reference_count_to_i32(page.reference_count)?))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut builder = QueryBuilder::<Postgres>::new(format!(
                "INSERT INTO {} \
                 (scope, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content) ",
                backend.table_name
            ));
            builder.push_values(prepared_batch, |mut row, (path, page, reference_count)| {
                row.push_bind(&scope_key)
                    .push_bind(path.as_str())
                    .push_bind(&page.title)
                    .push_bind(page_type_as_str(&page.page_type))
                    .push_bind(confidence_as_str(&page.confidence))
                    .push_bind(page.created)
                    .push_bind(page.updated)
                    .push_bind(page.last_referenced)
                    .push_bind(reference_count)
                    .push_bind(&page.tags)
                    .push_bind(&page.content);
            });
            builder
                .build()
                .execute(&mut *tx)
                .await
                .map_err(memory_error)?;
        }

        if backend.embedder.is_some() && !pages.is_empty() {
            let enqueue_sql = enqueue_page_sql(&backend.queue_table_name);
            for (path, _page) in pages {
                sqlx::query(&enqueue_sql)
                    .bind(&scope_key)
                    .bind(path.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(memory_error)?;
            }
        }

        tx.commit().await.map_err(memory_error)?;
        refresh_queue_depth(backend).await?;
        Ok(())
    }

    /// Runs one embedding-worker batch immediately.
    pub async fn run_embedding_queue_once(&self) -> Result<usize> {
        let Some(backend) = &self.backend else {
            return Ok(0);
        };
        self.run_embedding_queue_once_backend(backend).await
    }

    /// Enqueues every page in one scope for embedding or re-embedding.
    pub async fn enqueue_scope_embeddings(&self, scope: &MemoryScope) -> Result<u64> {
        let Some(backend) = &self.backend else {
            return Ok(0);
        };
        if backend.embedder.is_none() {
            return Ok(0);
        }

        let count = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {} WHERE scope = $1",
            backend.table_name
        ))
        .bind(scope_key(scope))
        .fetch_one(&*backend.pool)
        .await
        .map_err(memory_error)? as u64;

        sqlx::query(&format!(
            "INSERT INTO {} (scope, path) \
             SELECT scope, path FROM {} WHERE scope = $1 \
             ON CONFLICT (scope, path) DO UPDATE \
             SET enqueued_at = NOW(), attempt_count = 0, last_error = NULL",
            backend.queue_table_name, backend.table_name
        ))
        .bind(scope_key(scope))
        .execute(&*backend.pool)
        .await
        .map_err(memory_error)?;

        refresh_queue_depth(backend).await?;
        Ok(count)
    }

    /// Returns embedding queue and model diagnostics for doctor output.
    pub async fn embedding_status(&self) -> Result<EmbeddingIndexStatus> {
        let Some(backend) = &self.backend else {
            return Ok(EmbeddingIndexStatus {
                configured_model: None,
                missing_embeddings: 0,
                queue_depth: 0,
                mismatched_model_pages: 0,
                stored_models: Vec::new(),
            });
        };

        let missing_embeddings = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {} WHERE embedding IS NULL",
            backend.table_name
        ))
        .fetch_one(&*backend.pool)
        .await
        .map_err(memory_error)? as u64;
        let queue_depth = queue_depth(backend).await?;
        let configured_model = backend
            .embedder
            .as_ref()
            .map(|embedder| embedder.model_id().to_string());
        let stored_models = sqlx::query(&format!(
            "SELECT DISTINCT embedding_model FROM {} \
             WHERE embedding_model IS NOT NULL ORDER BY embedding_model ASC",
            backend.table_name
        ))
        .fetch_all(&*backend.pool)
        .await
        .map_err(memory_error)?
        .into_iter()
        .filter_map(|row| {
            row.try_get::<Option<String>, _>("embedding_model")
                .ok()
                .flatten()
        })
        .collect::<Vec<_>>();
        let mismatched_model_pages = match configured_model.as_deref() {
            Some(model_id) => sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} \
                 WHERE embedding IS NOT NULL AND embedding_model IS DISTINCT FROM $1",
                backend.table_name
            ))
            .bind(model_id)
            .fetch_one(&*backend.pool)
            .await
            .map_err(memory_error)? as u64,
            None => 0,
        };

        Ok(EmbeddingIndexStatus {
            configured_model,
            missing_embeddings,
            queue_depth,
            mismatched_model_pages,
            stored_models,
        })
    }

    fn backend(&self) -> Result<&WikiSearchBackend> {
        self.backend
            .as_deref()
            .ok_or_else(|| MoaError::NotImplemented(SEARCH_DISABLED_MESSAGE.to_string()))
    }

    async fn new_with_backend(
        pool: Arc<PgPool>,
        schema_name: Option<&str>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<Self> {
        schema::migrate(&pool, schema_name).await?;
        ensure_metrics_registered();
        let backend = Arc::new(WikiSearchBackend {
            pool,
            table_name: qualified_table_name(schema_name),
            queue_table_name: qualified_queue_table_name(schema_name),
            embedder,
            worker_started: AtomicBool::new(false),
        });
        refresh_queue_depth(&backend).await?;

        Ok(Self {
            backend: Some(backend),
        })
    }

    async fn search_hybrid(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let started_at = Instant::now();
        let fetch_limit = limit.saturating_mul(HYBRID_FETCH_MULTIPLIER).max(limit);
        let (keyword_result, semantic_result) = tokio::join!(
            self.search_keyword(backend, query, scope, fetch_limit),
            self.search_semantic(backend, query, scope, fetch_limit),
        );
        let keyword_results = keyword_result?;
        let semantic_results = match semantic_result {
            Ok(results) => results,
            Err(error) => {
                tracing::warn!(%error, "semantic memory search failed; falling back to keyword results");
                Vec::new()
            }
        };

        let fused = reciprocal_rank_fusion(&keyword_results, &semantic_results, HYBRID_RRF_K);
        record_search_latency("fusion", started_at.elapsed());
        Ok(fused.into_iter().take(limit).collect())
    }

    async fn search_keyword(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let primary = self.search_tsvector(backend, query, scope, limit).await?;
        if !primary.is_empty() {
            return Ok(primary);
        }

        if query.split_whitespace().count() <= 3 {
            return self.search_trigram(backend, query, scope, limit).await;
        }

        Ok(Vec::new())
    }

    async fn search_tsvector(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let started_at = Instant::now();
        let sql = format!(
            r#"
            WITH search_query AS (
                SELECT websearch_to_tsquery('english', $1) AS tsquery
            )
            SELECT
                path,
                title,
                page_type,
                confidence,
                updated,
                reference_count,
                COALESCE(
                    NULLIF(
                        ts_headline(
                            'english',
                            content,
                            search_query.tsquery,
                            'StartSel=<mark>, StopSel=</mark>, MaxFragments=2, MaxWords=20, MinWords=5'
                        ),
                        ''
                    ),
                    title
                ) AS snippet,
                ts_rank_cd(search_tsv, search_query.tsquery)
                    * CASE WHEN updated > NOW() - INTERVAL '7 days' THEN 2.0 ELSE 1.0 END
                    * CASE confidence WHEN 'high' THEN 3.0 WHEN 'medium' THEN 2.0 ELSE 1.0 END
                    * GREATEST(1.0, LOG((1 + reference_count)::double precision))
                    AS score
            FROM {table_name}, search_query
            WHERE scope = $2
              AND search_tsv @@ search_query.tsquery
            ORDER BY score DESC, updated DESC, path ASC
            LIMIT $3
            "#,
            table_name = backend.table_name
        );

        let rows = sqlx::query(&sql)
            .bind(query)
            .bind(scope_key(scope))
            .bind(limit as i64)
            .fetch_all(&*backend.pool)
            .await
            .map_err(memory_error)?;
        record_search_latency("tsvector", started_at.elapsed());

        rows.into_iter()
            .map(|row| row_to_search_result(&row, scope))
            .collect()
    }

    async fn search_trigram(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let started_at = Instant::now();
        let sql = format!(
            r#"
            SELECT
                path,
                title,
                page_type,
                confidence,
                updated,
                reference_count,
                COALESCE(NULLIF(LEFT(content, 200), ''), title) AS snippet,
                GREATEST(
                    similarity(lower(title), lower($1)),
                    word_similarity(lower($1), lower(title))
                )
                    * CASE WHEN updated > NOW() - INTERVAL '7 days' THEN 2.0 ELSE 1.0 END
                    * CASE confidence WHEN 'high' THEN 3.0 WHEN 'medium' THEN 2.0 ELSE 1.0 END
                    * GREATEST(1.0, LOG((1 + reference_count)::double precision))
                    AS score
            FROM {table_name}
            WHERE scope = $2
              AND (
                    lower(title) % lower($1)
                    OR word_similarity(lower($1), lower(title)) > 0.15
              )
            ORDER BY score DESC, updated DESC, path ASC
            LIMIT $3
            "#,
            table_name = backend.table_name
        );

        let rows = sqlx::query(&sql)
            .bind(query)
            .bind(scope_key(scope))
            .bind(limit as i64)
            .fetch_all(&*backend.pool)
            .await
            .map_err(memory_error)?;
        record_search_latency("trigram", started_at.elapsed());

        rows.into_iter()
            .map(|row| row_to_search_result(&row, scope))
            .collect()
    }

    async fn search_semantic(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let Some(embedder) = backend.embedder.as_ref() else {
            return Ok(Vec::new());
        };
        let started_at = Instant::now();
        let embeddings = embedder.embed(&[query.to_string()]).await?;
        let query_embedding = embeddings.into_iter().next().ok_or_else(|| {
            MoaError::ProviderError("embedding provider returned zero query embeddings".to_string())
        })?;
        if query_embedding.len() != embedder.dimensions() {
            return Err(MoaError::ProviderError(format!(
                "embedding dimension mismatch: expected {}, got {}",
                embedder.dimensions(),
                query_embedding.len()
            )));
        }

        let sql = format!(
            r#"
            SELECT
                path,
                title,
                page_type,
                confidence,
                updated,
                reference_count,
                LEFT(content, 220) AS snippet,
                content,
                (1 - (embedding <=> $1::vector))
                    * CASE WHEN updated > NOW() - INTERVAL '7 days' THEN 2.0 ELSE 1.0 END
                    * CASE confidence WHEN 'high' THEN 3.0 WHEN 'medium' THEN 2.0 ELSE 1.0 END
                    * GREATEST(1.0, LOG((1 + reference_count)::double precision))
                    AS score
            FROM {table_name}
            WHERE scope = $2
              AND embedding IS NOT NULL
              AND embedding_model = $3
            ORDER BY score DESC, updated DESC, path ASC
            LIMIT $4
            "#,
            table_name = backend.table_name
        );
        let rows = sqlx::query(&sql)
            .bind(vector_literal(&query_embedding))
            .bind(scope_key(scope))
            .bind(embedder.model_id())
            .bind(limit as i64)
            .fetch_all(&*backend.pool)
            .await
            .map_err(memory_error)?;
        record_search_latency("semantic", started_at.elapsed());

        rows.into_iter()
            .map(|row| {
                let mut result = row_to_search_result(&row, scope)?;
                let content = row.try_get::<String, _>("content").map_err(memory_error)?;
                result.snippet = semantic_snippet(&content, query);
                Ok(result)
            })
            .collect()
    }

    async fn run_embedding_queue_once_backend(&self, backend: &WikiSearchBackend) -> Result<usize> {
        let Some(embedder) = backend.embedder.as_ref() else {
            return Ok(0);
        };

        let mut tx = backend.pool.begin().await.map_err(memory_error)?;
        let claimed_rows = sqlx::query(&claim_embedding_batch_sql(
            &backend.table_name,
            &backend.queue_table_name,
        ))
        .bind(EMBEDDING_BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await
        .map_err(memory_error)?;

        if claimed_rows.is_empty() {
            tx.commit().await.map_err(memory_error)?;
            refresh_queue_depth(backend).await?;
            return Ok(0);
        }

        let mut stale_rows = Vec::new();
        let mut ready_rows = Vec::new();
        for row in claimed_rows {
            let scope = row.try_get::<String, _>("scope").map_err(memory_error)?;
            let path = row.try_get::<String, _>("path").map_err(memory_error)?;
            let title = row
                .try_get::<Option<String>, _>("title")
                .map_err(memory_error)?;
            let content = row
                .try_get::<Option<String>, _>("content")
                .map_err(memory_error)?;

            match (title, content) {
                (Some(title), Some(content)) => ready_rows.push(QueuedEmbeddingPage {
                    scope,
                    path,
                    title,
                    content,
                }),
                _ => stale_rows.push((scope, path)),
            }
        }

        if !stale_rows.is_empty() {
            delete_queue_rows(&mut tx, &backend.queue_table_name, &stale_rows).await?;
        }

        if ready_rows.is_empty() {
            tx.commit().await.map_err(memory_error)?;
            refresh_queue_depth(backend).await?;
            return Ok(stale_rows.len());
        }

        let inputs = ready_rows
            .iter()
            .map(|row| format!("{}\n\n{}", row.title, row.content))
            .collect::<Vec<_>>();
        match embedder.embed(&inputs).await {
            Ok(vectors) => {
                if vectors.len() != ready_rows.len() {
                    return Err(MoaError::ProviderError(format!(
                        "embedding response length mismatch: expected {}, got {}",
                        ready_rows.len(),
                        vectors.len()
                    )));
                }

                let timestamp = chrono::Utc::now();
                for (row, vector) in ready_rows.iter().zip(vectors.iter()) {
                    if vector.len() != embedder.dimensions() {
                        return Err(MoaError::ProviderError(format!(
                            "embedding dimension mismatch for {}: expected {}, got {}",
                            row.path,
                            embedder.dimensions(),
                            vector.len()
                        )));
                    }

                    sqlx::query(&update_embedding_sql(&backend.table_name))
                        .bind(vector_literal(vector))
                        .bind(embedder.model_id())
                        .bind(timestamp)
                        .bind(&row.scope)
                        .bind(&row.path)
                        .execute(&mut *tx)
                        .await
                        .map_err(memory_error)?;
                }

                let processed_rows = ready_rows
                    .iter()
                    .map(|row| (row.scope.clone(), row.path.clone()))
                    .collect::<Vec<_>>();
                delete_queue_rows(&mut tx, &backend.queue_table_name, &processed_rows).await?;
                tx.commit().await.map_err(memory_error)?;
                embeddings_computed_counter().add(
                    ready_rows.len() as u64,
                    &[opentelemetry::KeyValue::new(
                        "model",
                        embedder.model_id().to_string(),
                    )],
                );
                refresh_queue_depth(backend).await?;
                Ok(stale_rows.len() + ready_rows.len())
            }
            Err(error) => {
                mark_queue_failures(
                    &mut tx,
                    &backend.queue_table_name,
                    &ready_rows
                        .iter()
                        .map(|row| (row.scope.clone(), row.path.clone()))
                        .collect::<Vec<_>>(),
                    &error.to_string(),
                )
                .await?;
                tx.commit().await.map_err(memory_error)?;
                embedding_failures_counter().add(
                    ready_rows.len() as u64,
                    &[opentelemetry::KeyValue::new(
                        "error",
                        classify_embedding_error(&error),
                    )],
                );
                refresh_queue_depth(backend).await?;
                Err(error)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct QueuedEmbeddingPage {
    scope: String,
    path: String,
    title: String,
    content: String,
}

/// Fuses two ranked result lists with reciprocal rank fusion.
pub fn reciprocal_rank_fusion(
    keyword_results: &[MemorySearchResult],
    semantic_results: &[MemorySearchResult],
    k: u32,
) -> Vec<MemorySearchResult> {
    let mut fused: HashMap<(MemoryScope, MemoryPath), (f64, MemorySearchResult)> = HashMap::new();

    for (rank, result) in keyword_results.iter().enumerate() {
        add_rrf_score(&mut fused, result, rank, k);
    }
    for (rank, result) in semantic_results.iter().enumerate() {
        add_rrf_score(&mut fused, result, rank, k);
    }

    let mut values = fused.into_values().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.1.updated.cmp(&left.1.updated))
            .then_with(|| left.1.path.as_str().cmp(right.1.path.as_str()))
    });
    values.into_iter().map(|(_score, result)| result).collect()
}

fn add_rrf_score(
    fused: &mut HashMap<(MemoryScope, MemoryPath), (f64, MemorySearchResult)>,
    result: &MemorySearchResult,
    rank: usize,
    k: u32,
) {
    let key = (result.scope.clone(), result.path.clone());
    let score = 1.0 / (f64::from(k) + rank as f64 + 1.0);
    fused
        .entry(key)
        .and_modify(|(existing_score, _existing_result)| *existing_score += score)
        .or_insert_with(|| (score, result.clone()));
}

fn row_to_search_result(
    row: &sqlx::postgres::PgRow,
    scope: &MemoryScope,
) -> Result<MemorySearchResult> {
    let path = row.try_get::<String, _>("path").map_err(memory_error)?;
    let title = row.try_get::<String, _>("title").map_err(memory_error)?;
    let page_type = parse_page_type(
        row.try_get::<String, _>("page_type")
            .map_err(memory_error)?
            .as_str(),
    )?;
    let confidence = parse_confidence(
        row.try_get::<String, _>("confidence")
            .map_err(memory_error)?
            .as_str(),
    )?;
    let snippet = row
        .try_get::<Option<String>, _>("snippet")
        .map_err(memory_error)?
        .unwrap_or_else(|| title.clone());
    let updated = row
        .try_get::<chrono::DateTime<chrono::Utc>, _>("updated")
        .map_err(memory_error)?;
    let reference_count = row
        .try_get::<i32, _>("reference_count")
        .map_err(memory_error)? as u64;

    Ok(MemorySearchResult {
        scope: scope.clone(),
        path: MemoryPath::new(path),
        title,
        page_type,
        snippet,
        confidence,
        updated,
        reference_count,
    })
}

fn upsert_sql(table_name: &str) -> String {
    format!(
        r#"
        INSERT INTO {table_name}
            (scope, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (scope, path) DO UPDATE SET
            title = EXCLUDED.title,
            page_type = EXCLUDED.page_type,
            confidence = EXCLUDED.confidence,
            updated = EXCLUDED.updated,
            last_referenced = EXCLUDED.last_referenced,
            reference_count = EXCLUDED.reference_count,
            tags = EXCLUDED.tags,
            content = EXCLUDED.content,
            embedding = NULL,
            embedding_model = NULL,
            embedding_updated = NULL
        "#
    )
}

fn enqueue_page_sql(queue_table_name: &str) -> String {
    format!(
        "INSERT INTO {} (scope, path) VALUES ($1, $2) \
         ON CONFLICT (scope, path) DO UPDATE \
         SET enqueued_at = NOW(), attempt_count = 0, last_error = NULL",
        queue_table_name
    )
}

fn claim_embedding_batch_sql(table_name: &str, queue_table_name: &str) -> String {
    format!(
        r#"
        SELECT
            q.scope,
            q.path,
            p.title,
            p.content
        FROM {queue_table_name} q
        LEFT JOIN {table_name} p
            ON p.scope = q.scope AND p.path = q.path
        WHERE q.attempt_count < 5
        ORDER BY q.enqueued_at ASC
        LIMIT $1
        FOR UPDATE OF q SKIP LOCKED
        "#
    )
}

fn update_embedding_sql(table_name: &str) -> String {
    format!(
        "UPDATE {} SET embedding = $1::vector, embedding_model = $2, embedding_updated = $3 \
         WHERE scope = $4 AND path = $5",
        table_name
    )
}

async fn delete_queue_rows(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    queue_table_name: &str,
    rows: &[(String, String)],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    for (scope, path) in rows {
        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1 AND path = $2",
            queue_table_name
        ))
        .bind(scope)
        .bind(path)
        .execute(&mut **tx)
        .await
        .map_err(memory_error)?;
    }

    Ok(())
}

async fn mark_queue_failures(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    queue_table_name: &str,
    rows: &[(String, String)],
    error: &str,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    for (scope, path) in rows {
        sqlx::query(&format!(
            "UPDATE {} \
             SET attempt_count = attempt_count + 1, last_error = $3, enqueued_at = NOW() \
             WHERE scope = $1 AND path = $2",
            queue_table_name
        ))
        .bind(scope)
        .bind(path)
        .bind(error)
        .execute(&mut **tx)
        .await
        .map_err(memory_error)?;
    }

    Ok(())
}

async fn queue_depth(backend: &WikiSearchBackend) -> Result<u64> {
    Ok(sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM {}",
        backend.queue_table_name
    ))
    .fetch_one(&*backend.pool)
    .await
    .map_err(memory_error)? as u64)
}

async fn refresh_queue_depth(backend: &WikiSearchBackend) -> Result<()> {
    let depth = queue_depth(backend).await?;
    EMBEDDING_QUEUE_DEPTH.store(depth, AtomicOrdering::Relaxed);
    record_embedding_queue_depth(depth);
    Ok(())
}

fn spawn_embedding_worker(backend: Arc<WikiSearchBackend>) {
    tokio::spawn(async move {
        loop {
            let index = WikiSearchIndex {
                backend: Some(backend.clone()),
            };
            match index.run_embedding_queue_once_backend(&backend).await {
                Ok(0) => sleep(EMBEDDING_IDLE_DELAY).await,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "embedding worker batch failed");
                    sleep(EMBEDDING_ERROR_DELAY).await;
                }
            }
        }
    });
}

fn vector_literal(vector: &[f32]) -> String {
    let mut literal = String::from("[");
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            literal.push(',');
        }
        literal.push_str(&format!("{value:.8}"));
    }
    literal.push(']');
    literal
}

fn semantic_snippet(content: &str, query: &str) -> String {
    let lowered_content = content.to_ascii_lowercase();
    let tokens = query
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric()))
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();

    for token in &tokens {
        if let Some(position) = lowered_content.find(token) {
            return slice_snippet(content, position);
        }
    }

    let trimmed = content.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        slice_snippet(trimmed, 0)
    }
}

fn slice_snippet(content: &str, start: usize) -> String {
    let start = start.saturating_sub(80);
    let end = (start + 220).min(content.len());
    let snippet = content
        .get(start..end)
        .unwrap_or(content)
        .trim()
        .replace('\n', " ");
    if end < content.len() {
        format!("{snippet}...")
    } else {
        snippet
    }
}

fn record_search_latency(component: &'static str, duration: Duration) {
    search_latency_histogram().record(
        duration.as_secs_f64(),
        &[opentelemetry::KeyValue::new("component", component)],
    );
}

fn classify_embedding_error(error: &MoaError) -> String {
    match error {
        MoaError::HttpStatus { .. } => "http".to_string(),
        MoaError::MissingEnvironmentVariable(_) => "missing_env".to_string(),
        MoaError::RateLimited { .. } => "rate_limited".to_string(),
        MoaError::ProviderError(_) => "provider".to_string(),
        _ => "other".to_string(),
    }
}

fn ensure_metrics_registered() {
    let _ = embedding_queue_depth_gauge();
    let _ = embeddings_computed_counter();
    let _ = embedding_failures_counter();
    let _ = search_latency_histogram();
}

static EMBEDDING_QUEUE_DEPTH: AtomicU64 = AtomicU64::new(0);

fn embedding_queue_depth_gauge() -> &'static ObservableGauge<u64> {
    static GAUGE: OnceLock<ObservableGauge<u64>> = OnceLock::new();
    GAUGE.get_or_init(|| {
        global::meter("moa.memory")
            .u64_observable_gauge("moa_embedding_queue_depth")
            .with_description("Approximate number of wiki pages waiting for embeddings.")
            .with_callback(|observer| {
                observer.observe(EMBEDDING_QUEUE_DEPTH.load(AtomicOrdering::Relaxed), &[])
            })
            .build()
    })
}

fn embeddings_computed_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("moa.memory")
            .u64_counter("moa_embeddings_computed_total")
            .with_description("Number of wiki page embeddings successfully persisted.")
            .build()
    })
}

fn embedding_failures_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        global::meter("moa.memory")
            .u64_counter("moa_embedding_failures_total")
            .with_description("Number of wiki page embedding attempts that failed.")
            .build()
    })
}

fn search_latency_histogram() -> &'static Histogram<f64> {
    static HISTOGRAM: OnceLock<Histogram<f64>> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        global::meter("moa.memory")
            .f64_histogram("moa_search_hybrid_latency_seconds")
            .with_description("Latency for keyword, semantic, and hybrid wiki search components.")
            .build()
    })
}

fn qualified_table_name(schema_name: Option<&str>) -> String {
    schema_name
        .map(|schema_name| qualified_name(schema_name, "wiki_pages"))
        .unwrap_or_else(|| "wiki_pages".to_string())
}

fn qualified_queue_table_name(schema_name: Option<&str>) -> String {
    schema_name
        .map(|schema_name| qualified_name(schema_name, "wiki_embedding_queue"))
        .unwrap_or_else(|| "wiki_embedding_queue".to_string())
}

fn qualified_name(schema_name: &str, object_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(object_name)
    )
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn scope_key(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::User(_) => "user".to_string(),
        MemoryScope::Workspace(workspace_id) => format!("workspace:{}", workspace_id.as_str()),
    }
}

fn page_type_as_str(page_type: &PageType) -> &'static str {
    match page_type {
        PageType::Index => "index",
        PageType::Topic => "topic",
        PageType::Entity => "entity",
        PageType::Decision => "decision",
        PageType::Skill => "skill",
        PageType::Source => "source",
        PageType::Schema => "schema",
        PageType::Log => "log",
    }
}

fn parse_page_type(value: &str) -> Result<PageType> {
    match value {
        "index" => Ok(PageType::Index),
        "topic" => Ok(PageType::Topic),
        "entity" => Ok(PageType::Entity),
        "decision" => Ok(PageType::Decision),
        "skill" => Ok(PageType::Skill),
        "source" => Ok(PageType::Source),
        "schema" => Ok(PageType::Schema),
        "log" => Ok(PageType::Log),
        _ => Err(memory_error(format!("unknown wiki page type `{value}`"))),
    }
}

fn confidence_as_str(confidence: &ConfidenceLevel) -> &'static str {
    match confidence {
        ConfidenceLevel::High => "high",
        ConfidenceLevel::Medium => "medium",
        ConfidenceLevel::Low => "low",
    }
}

fn parse_confidence(value: &str) -> Result<ConfidenceLevel> {
    match value {
        "high" => Ok(ConfidenceLevel::High),
        "medium" => Ok(ConfidenceLevel::Medium),
        "low" => Ok(ConfidenceLevel::Low),
        _ => Err(memory_error(format!(
            "unknown wiki page confidence `{value}`"
        ))),
    }
}

fn reference_count_to_i32(reference_count: u64) -> Result<i32> {
    i32::try_from(reference_count)
        .map_err(|_| memory_error(format!("reference_count {reference_count} exceeds i32")))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{ConfidenceLevel, MemoryScope, PageType};

    use super::{HYBRID_RRF_K, reciprocal_rank_fusion, semantic_snippet};

    fn result(path: &str, updated_hour: u32) -> moa_core::MemorySearchResult {
        moa_core::MemorySearchResult {
            scope: MemoryScope::Workspace("ws".into()),
            path: path.into(),
            title: path.to_string(),
            page_type: PageType::Topic,
            snippet: String::from("snippet"),
            confidence: ConfidenceLevel::High,
            updated: Utc
                .with_ymd_and_hms(2026, 4, 17, updated_hour, 0, 0)
                .unwrap(),
            reference_count: 1,
        }
    }

    #[test]
    fn reciprocal_rank_fusion_combines_overlapping_lists() {
        let keyword = vec![result("topics/oauth.md", 2), result("topics/cache.md", 1)];
        let semantic = vec![result("topics/cache.md", 3), result("topics/oauth.md", 1)];

        let fused = reciprocal_rank_fusion(&keyword, &semantic, HYBRID_RRF_K);

        assert_eq!(fused.len(), 2);
        assert!(
            fused
                .iter()
                .any(|result| result.path.as_str() == "topics/oauth.md")
        );
        assert!(
            fused
                .iter()
                .any(|result| result.path.as_str() == "topics/cache.md")
        );
    }

    #[test]
    fn semantic_snippet_falls_back_to_prefix_when_terms_do_not_match() {
        let snippet = semantic_snippet(
            "OAuth refresh tokens rotate on every successful refresh request.",
            "thing we discussed last week",
        );

        assert!(snippet.contains("OAuth refresh tokens"));
    }
}
