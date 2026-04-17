//! Postgres-backed wiki search index with `tsvector` + GIN ranking.

use std::sync::Arc;

use moa_core::{
    ConfidenceLevel, MemoryPath, MemoryScope, MemorySearchResult, MoaError, PageType, Result,
    WikiPage,
};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use crate::{memory_error, schema};

const SEARCH_DISABLED_MESSAGE: &str =
    "wiki search requires the Postgres tsvector index — see step 90";
const REBUILD_BATCH_SIZE: usize = 256;

#[derive(Clone)]
struct WikiSearchBackend {
    pool: Arc<PgPool>,
    table_name: String,
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
        schema::migrate(&pool, schema_name).await?;
        Ok(Self {
            backend: Some(Arc::new(WikiSearchBackend {
                pool,
                table_name: qualified_table_name(schema_name),
            })),
        })
    }

    /// Searches wiki content within a memory scope.
    pub async fn search(
        &self,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let q = query.trim();
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let backend = self.backend()?;
        let primary = self.search_tsvector(backend, q, scope, limit).await?;
        if !primary.is_empty() {
            return Ok(primary);
        }

        if q.split_whitespace().count() <= 3 {
            return self.search_trigram(backend, q, scope, limit).await;
        }

        Ok(Vec::new())
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

        sqlx::query(&format!(
            "DELETE FROM {} WHERE scope = $1 AND path = $2",
            backend.table_name
        ))
        .bind(scope_key(scope))
        .bind(path.as_str())
        .execute(&*backend.pool)
        .await
        .map_err(memory_error)?;

        Ok(())
    }

    /// Rebuilds the search index for one scope.
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

        tx.commit().await.map_err(memory_error)?;
        Ok(())
    }

    fn backend(&self) -> Result<&WikiSearchBackend> {
        self.backend
            .as_deref()
            .ok_or_else(|| MoaError::NotImplemented(SEARCH_DISABLED_MESSAGE.to_string()))
    }

    async fn search_tsvector(
        &self,
        backend: &WikiSearchBackend,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
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

        rows.into_iter()
            .map(|row| row_to_search_result(&row, scope))
            .collect()
    }
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
            content = EXCLUDED.content
        "#
    )
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
