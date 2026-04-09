//! FTS5-backed search index for wiki memory pages.

use std::path::Path;

use chrono::{DateTime, Utc};
use libsql::{Builder, Connection, TransactionBehavior, params};
use moa_core::{
    ConfidenceLevel, MemoryPath, MemoryScope, MemorySearchResult, MoaError, PageType, Result,
    WikiPage,
};
use tokio::fs;

use crate::memory_error;

/// DDL for the wiki search virtual table.
pub const CREATE_WIKI_SEARCH_TABLE: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS wiki_search USING fts5(
    path,
    scope,
    title,
    page_type,
    tags,
    content,
    tokenize='porter unicode61'
);
"#;

/// DDL for the wiki metadata table.
pub const CREATE_WIKI_PAGES_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS wiki_pages (
    path TEXT NOT NULL,
    scope TEXT NOT NULL,
    title TEXT NOT NULL,
    page_type TEXT NOT NULL,
    confidence TEXT NOT NULL,
    created TEXT NOT NULL,
    updated TEXT NOT NULL,
    last_referenced TEXT NOT NULL,
    reference_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, path)
);
"#;

/// Local FTS5 index wrapper.
#[derive(Clone)]
pub struct FtsIndex {
    connection: Connection,
}

impl FtsIndex {
    /// Opens or creates a local FTS index database.
    pub async fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let database = Builder::new_local(path)
            .build()
            .await
            .map_err(memory_error)?;
        let connection = database.connect().map_err(memory_error)?;
        migrate(&connection).await?;

        Ok(Self { connection })
    }

    /// Rebuilds all indexed pages for a single scope.
    pub async fn rebuild_scope(
        &self,
        scope: &MemoryScope,
        pages: &[(MemoryPath, WikiPage)],
    ) -> Result<()> {
        let scope_key = scope_key(scope);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(memory_error)?;

        transaction
            .execute(
                "DELETE FROM wiki_search WHERE scope = ?",
                [scope_key.clone()],
            )
            .await
            .map_err(memory_error)?;
        transaction
            .execute(
                "DELETE FROM wiki_pages WHERE scope = ?",
                [scope_key.clone()],
            )
            .await
            .map_err(memory_error)?;

        for (path, page) in pages {
            insert_page(&transaction, &scope_key, path, page).await?;
        }

        transaction.commit().await.map_err(memory_error)?;

        Ok(())
    }

    /// Updates the index entry for a single page.
    pub async fn upsert_page(
        &self,
        scope: &MemoryScope,
        path: &MemoryPath,
        page: &WikiPage,
    ) -> Result<()> {
        let scope_key = scope_key(scope);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(memory_error)?;

        delete_page_entries(&transaction, &scope_key, path).await?;
        insert_page(&transaction, &scope_key, path, page).await?;
        transaction.commit().await.map_err(memory_error)?;

        Ok(())
    }

    /// Removes a page from the search index.
    pub async fn delete_page(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<()> {
        let scope_key = scope_key(scope);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(memory_error)?;

        delete_page_entries(&transaction, &scope_key, path).await?;
        transaction.commit().await.map_err(memory_error)?;

        Ok(())
    }

    /// Executes an FTS query within a single memory scope.
    pub async fn search(
        &self,
        query: &str,
        scope: &MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        let normalized_query = query.trim();
        if normalized_query.is_empty() {
            return Ok(Vec::new());
        }

        let sql = r#"
SELECT path, title, page_type, snippet, confidence, updated, reference_count
FROM (
    SELECT
        ws.path AS path,
        ws.title AS title,
        ws.page_type AS page_type,
        snippet(wiki_search, 5, '<mark>', '</mark>', '...', 40) AS snippet,
        wp.confidence AS confidence,
        wp.updated AS updated,
        wp.reference_count AS reference_count,
        bm25(wiki_search) AS score
    FROM wiki_search ws
    JOIN wiki_pages wp ON ws.path = wp.path AND ws.scope = wp.scope
    WHERE wiki_search MATCH ? AND ws.scope = ?
)
ORDER BY
    CASE WHEN updated > datetime('now', '-7 days') THEN score * 0.5 ELSE score END ASC,
    CASE confidence WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END ASC,
    reference_count DESC
LIMIT ?
"#;
        let mut rows = self
            .connection
            .query(
                sql,
                params![normalized_query, scope_key(scope), limit as i64],
            )
            .await
            .map_err(memory_error)?;
        let mut results = Vec::new();

        while let Some(row) = rows.next().await.map_err(memory_error)? {
            let updated_raw: String = row.get(5).map_err(memory_error)?;
            results.push(MemorySearchResult {
                path: MemoryPath::new(row.get::<String>(0).map_err(memory_error)?),
                title: row.get(1).map_err(memory_error)?,
                page_type: parse_page_type(&row.get::<String>(2).map_err(memory_error)?)?,
                snippet: row.get(3).map_err(memory_error)?,
                confidence: parse_confidence(&row.get::<String>(4).map_err(memory_error)?)?,
                updated: parse_timestamp(&updated_raw)?,
                reference_count: row.get::<i64>(6).map_err(memory_error)? as u64,
            });
        }

        Ok(results)
    }

    /// Finds all indexed scopes for a logical path.
    pub async fn scopes_for_path(&self, path: &MemoryPath) -> Result<Vec<MemoryScope>> {
        let mut rows = self
            .connection
            .query(
                "SELECT scope FROM wiki_pages WHERE path = ? ORDER BY scope ASC",
                [path.as_str().to_string()],
            )
            .await
            .map_err(memory_error)?;
        let mut scopes = Vec::new();

        while let Some(row) = rows.next().await.map_err(memory_error)? {
            let scope_raw: String = row.get(0).map_err(memory_error)?;
            scopes.push(parse_scope_key(&scope_raw)?);
        }

        Ok(scopes)
    }
}

/// Creates the FTS tables if they do not exist yet.
pub async fn migrate(connection: &Connection) -> Result<()> {
    for statement in [CREATE_WIKI_SEARCH_TABLE, CREATE_WIKI_PAGES_TABLE] {
        connection
            .execute_batch(statement)
            .await
            .map_err(memory_error)?;
    }

    Ok(())
}

pub(crate) fn scope_key(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::User(user_id) => format!("user:{user_id}"),
        MemoryScope::Workspace(workspace_id) => format!("workspace:{workspace_id}"),
    }
}

pub(crate) fn parse_scope_key(raw: &str) -> Result<MemoryScope> {
    match raw.split_once(':') {
        Some(("user", id)) => Ok(MemoryScope::User(id.into())),
        Some(("workspace", id)) => Ok(MemoryScope::Workspace(id.into())),
        _ => Err(MoaError::ValidationError(format!(
            "invalid memory scope key: {raw}"
        ))),
    }
}

async fn delete_page_entries(
    transaction: &libsql::Transaction,
    scope_key: &str,
    path: &MemoryPath,
) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM wiki_search WHERE scope = ? AND path = ?",
            params![scope_key, path.as_str()],
        )
        .await
        .map_err(memory_error)?;
    transaction
        .execute(
            "DELETE FROM wiki_pages WHERE scope = ? AND path = ?",
            params![scope_key, path.as_str()],
        )
        .await
        .map_err(memory_error)?;

    Ok(())
}

async fn insert_page(
    transaction: &libsql::Transaction,
    scope_key: &str,
    path: &MemoryPath,
    page: &WikiPage,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO wiki_search (path, scope, title, page_type, tags, content) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                path.as_str(),
                scope_key,
                page.title.as_str(),
                page_type_to_db(&page.page_type),
                page.tags.join(" "),
                page.content.as_str(),
            ],
        )
        .await
        .map_err(memory_error)?;
    transaction
        .execute(
            "INSERT INTO wiki_pages (path, scope, title, page_type, confidence, created, updated, last_referenced, reference_count) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                path.as_str(),
                scope_key,
                page.title.as_str(),
                page_type_to_db(&page.page_type),
                confidence_to_db(&page.confidence),
                page.created.to_rfc3339(),
                page.updated.to_rfc3339(),
                page.last_referenced.to_rfc3339(),
                page.reference_count as i64,
            ],
        )
        .await
        .map_err(memory_error)?;

    Ok(())
}

fn page_type_to_db(page_type: &PageType) -> &'static str {
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

fn parse_page_type(raw: &str) -> Result<PageType> {
    match raw {
        "index" => Ok(PageType::Index),
        "topic" => Ok(PageType::Topic),
        "entity" => Ok(PageType::Entity),
        "decision" => Ok(PageType::Decision),
        "skill" => Ok(PageType::Skill),
        "source" => Ok(PageType::Source),
        "schema" => Ok(PageType::Schema),
        "log" => Ok(PageType::Log),
        _ => Err(MoaError::ValidationError(format!(
            "invalid page type in index: {raw}"
        ))),
    }
}

fn confidence_to_db(confidence: &ConfidenceLevel) -> &'static str {
    match confidence {
        ConfidenceLevel::High => "high",
        ConfidenceLevel::Medium => "medium",
        ConfidenceLevel::Low => "low",
    }
}

fn parse_confidence(raw: &str) -> Result<ConfidenceLevel> {
    match raw {
        "high" => Ok(ConfidenceLevel::High),
        "medium" => Ok(ConfidenceLevel::Medium),
        "low" => Ok(ConfidenceLevel::Low),
        _ => Err(MoaError::ValidationError(format!(
            "invalid confidence in index: {raw}"
        ))),
    }
}

fn parse_timestamp(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(memory_error)
}
