//! Postgres-backed wiki search with keyword-only retrieval.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use moa_core::{
    ConfidenceLevel, MemoryPath, MemoryScope, MemorySearchMode, MemorySearchResult, MoaError,
    PageType, Result, WikiPage,
};
use opentelemetry::global;
use opentelemetry::metrics::Histogram;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::{memory_error, schema};

const SEARCH_DISABLED_MESSAGE: &str =
    "wiki search requires the Postgres tsvector index - see step 90";
const REBUILD_BATCH_SIZE: usize = 256;
#[cfg(test)]
const HYBRID_RRF_K: u32 = 60;

struct WikiSearchBackend {
    pool: Arc<PgPool>,
    table_name: String,
}

#[derive(Debug, Clone)]
struct ScopeKey {
    tier: &'static str,
    workspace_id: Option<String>,
    user_id: Option<String>,
}

/// Deprecated legacy embedding-index health snapshot.
///
/// Wiki embeddings moved out of `moa-memory`; this status is always disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingIndexStatus {
    /// Always `None` in the legacy shim.
    pub configured_model: Option<String>,
    /// Always zero in the legacy shim.
    pub missing_embeddings: u64,
    /// Always zero in the legacy shim.
    pub queue_depth: u64,
    /// Always zero in the legacy shim.
    pub mismatched_model_pages: u64,
    /// Always empty in the legacy shim.
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
        Self::new_with_backend(pool, schema_name).await
    }

    /// Creates a Postgres-backed wiki search index handle and ignores legacy embedders.
    #[deprecated(note = "wiki embeddings moved out of moa-memory; use moa-memory-vector")]
    pub async fn new_with_pool_and_embedder<T: Send + Sync + ?Sized + 'static>(
        pool: Arc<PgPool>,
        schema_name: Option<&str>,
        _embedder: Arc<T>,
    ) -> Result<Self> {
        Self::new_with_backend(pool, schema_name).await
    }

    /// No-op legacy worker hook retained until `moa-memory` is deleted.
    #[deprecated(note = "wiki embeddings moved out of moa-memory; use moa-memory-vector")]
    pub fn start_embedding_worker(&self) {}

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
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let backend = self.backend()?;
        match mode {
            MemorySearchMode::Keyword | MemorySearchMode::Hybrid => {
                self.search_keyword(backend, query, scope, limit).await
            }
            MemorySearchMode::Semantic => Ok(Vec::new()),
        }
    }

    /// Upserts one wiki page into the search index.
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
        let scope_key = scope_key(scope);

        sqlx::query(&upsert_sql(&backend.table_name))
            .bind(scope_key.workspace_id.as_deref())
            .bind(scope_key.user_id.as_deref())
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
            .execute(&*backend.pool)
            .await
            .map_err(memory_error)?;
        Ok(())
    }

    /// Removes one wiki page from the search index.
    pub async fn delete_page(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<()> {
        let Some(backend) = &self.backend else {
            return Ok(());
        };
        let scope_key = scope_key(scope);

        sqlx::query(&format!(
            "DELETE FROM {} \
             WHERE workspace_id IS NOT DISTINCT FROM $1 \
               AND user_id IS NOT DISTINCT FROM $2 \
               AND path = $3",
            backend.table_name
        ))
        .bind(scope_key.workspace_id.as_deref())
        .bind(scope_key.user_id.as_deref())
        .bind(path.as_str())
        .execute(&*backend.pool)
        .await
        .map_err(memory_error)?;

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
            "DELETE FROM {} \
             WHERE workspace_id IS NOT DISTINCT FROM $1 \
               AND user_id IS NOT DISTINCT FROM $2",
            backend.table_name
        ))
        .bind(scope_key.workspace_id.as_deref())
        .bind(scope_key.user_id.as_deref())
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
                 (workspace_id, user_id, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content) ",
                backend.table_name
            ));
            builder.push_values(prepared_batch, |mut row, (path, page, reference_count)| {
                row.push_bind(scope_key.workspace_id.as_deref())
                    .push_bind(scope_key.user_id.as_deref())
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

        tx.commit().await.map_err(memory_error)?;
        Ok(())
    }

    /// No-op legacy embedding queue drain retained until `moa-memory` is deleted.
    #[deprecated(note = "wiki embeddings moved out of moa-memory; use moa-memory-vector")]
    pub async fn run_embedding_queue_once(&self) -> Result<usize> {
        Ok(0)
    }

    /// No-op legacy embedding enqueue retained until `moa-memory` is deleted.
    #[deprecated(note = "wiki embeddings moved out of moa-memory; use moa-memory-vector")]
    pub async fn enqueue_scope_embeddings(&self, _scope: &MemoryScope) -> Result<u64> {
        Ok(0)
    }

    /// Returns disabled legacy embedding diagnostics.
    #[deprecated(note = "wiki embeddings moved out of moa-memory; use moa-memory-vector")]
    pub async fn embedding_status(&self) -> Result<EmbeddingIndexStatus> {
        Ok(EmbeddingIndexStatus {
            configured_model: None,
            missing_embeddings: 0,
            queue_depth: 0,
            mismatched_model_pages: 0,
            stored_models: Vec::new(),
        })
    }

    fn backend(&self) -> Result<&WikiSearchBackend> {
        self.backend
            .as_deref()
            .ok_or_else(|| MoaError::NotImplemented(SEARCH_DISABLED_MESSAGE.to_string()))
    }

    async fn new_with_backend(pool: Arc<PgPool>, schema_name: Option<&str>) -> Result<Self> {
        schema::migrate(&pool, schema_name).await?;
        ensure_metrics_registered();
        let backend = Arc::new(WikiSearchBackend {
            pool,
            table_name: qualified_table_name(schema_name),
        });

        Ok(Self {
            backend: Some(backend),
        })
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
        let scope_key = scope_key(scope);
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
              AND workspace_id IS NOT DISTINCT FROM $3
              AND user_id IS NOT DISTINCT FROM $4
              AND search_tsv @@ search_query.tsquery
            ORDER BY score DESC, updated DESC, path ASC
            LIMIT $5
            "#,
            table_name = backend.table_name
        );

        let rows = sqlx::query(&sql)
            .bind(query)
            .bind(scope_key.tier)
            .bind(scope_key.workspace_id.as_deref())
            .bind(scope_key.user_id.as_deref())
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
        let scope_key = scope_key(scope);
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
              AND workspace_id IS NOT DISTINCT FROM $3
              AND user_id IS NOT DISTINCT FROM $4
              AND (
                    lower(title) % lower($1)
                    OR word_similarity(lower($1), lower(title)) > 0.15
              )
            ORDER BY score DESC, updated DESC, path ASC
            LIMIT $5
            "#,
            table_name = backend.table_name
        );

        let rows = sqlx::query(&sql)
            .bind(query)
            .bind(scope_key.tier)
            .bind(scope_key.workspace_id.as_deref())
            .bind(scope_key.user_id.as_deref())
            .bind(limit as i64)
            .fetch_all(&*backend.pool)
            .await
            .map_err(memory_error)?;
        record_search_latency("trigram", started_at.elapsed());

        rows.into_iter()
            .map(|row| row_to_search_result(&row, scope))
            .collect()
    }
}

/// Fuses two ranked result lists with reciprocal rank fusion.
pub fn reciprocal_rank_fusion(
    keyword_results: &[MemorySearchResult],
    secondary_results: &[MemorySearchResult],
    k: u32,
) -> Vec<MemorySearchResult> {
    let mut fused: HashMap<(MemoryScope, MemoryPath), (f64, MemorySearchResult)> = HashMap::new();

    for (rank, result) in keyword_results.iter().enumerate() {
        add_rrf_score(&mut fused, result, rank, k);
    }
    for (rank, result) in secondary_results.iter().enumerate() {
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
            (workspace_id, user_id, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (workspace_id, user_id, path) DO UPDATE SET
            title = EXCLUDED.title,
            page_type = EXCLUDED.page_type,
            confidence = EXCLUDED.confidence,
            updated = EXCLUDED.updated,
            last_referenced = EXCLUDED.last_referenced,
            reference_count = EXCLUDED.reference_count,
            tags = EXCLUDED.tags,
            content = EXCLUDED.content
        "#
    )
}

fn record_search_latency(component: &'static str, duration: Duration) {
    search_latency_histogram().record(
        duration.as_secs_f64(),
        &[opentelemetry::KeyValue::new("component", component)],
    );
}

fn ensure_metrics_registered() {
    let _ = search_latency_histogram();
}

fn search_latency_histogram() -> &'static Histogram<f64> {
    static HISTOGRAM: OnceLock<Histogram<f64>> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        global::meter("moa.memory")
            .f64_histogram("moa_search_latency_seconds")
            .with_description("Latency for keyword wiki search components.")
            .build()
    })
}

fn qualified_table_name(schema_name: Option<&str>) -> String {
    schema_name
        .map(|schema_name| qualified_name(schema_name, "wiki_pages"))
        .unwrap_or_else(|| "wiki_pages".to_string())
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

fn scope_key(scope: &MemoryScope) -> ScopeKey {
    match scope {
        MemoryScope::Global => ScopeKey {
            tier: "global",
            workspace_id: None,
            user_id: None,
        },
        MemoryScope::Workspace { workspace_id } => ScopeKey {
            tier: "workspace",
            workspace_id: Some(workspace_id.to_string()),
            user_id: None,
        },
        MemoryScope::User {
            workspace_id,
            user_id,
        } => ScopeKey {
            tier: "user",
            workspace_id: Some(workspace_id.to_string()),
            user_id: Some(user_id.to_string()),
        },
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

    use super::{HYBRID_RRF_K, reciprocal_rank_fusion};

    fn result(path: &str, updated_hour: u32) -> moa_core::MemorySearchResult {
        moa_core::MemorySearchResult {
            scope: MemoryScope::Workspace {
                workspace_id: "ws".into(),
            },
            path: path.into(),
            title: path.to_string(),
            page_type: PageType::Topic,
            snippet: String::from("snippet"),
            confidence: ConfidenceLevel::High,
            updated: Utc
                .with_ymd_and_hms(2026, 4, 17, updated_hour, 0, 0)
                .single()
                .expect("valid timestamp"),
            reference_count: 1,
        }
    }

    #[test]
    fn reciprocal_rank_fusion_combines_overlapping_lists() {
        let keyword = vec![result("topics/oauth.md", 2), result("topics/cache.md", 1)];
        let secondary = vec![result("topics/cache.md", 3), result("topics/oauth.md", 1)];

        let fused = reciprocal_rank_fusion(&keyword, &secondary, HYBRID_RRF_K);

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
}
