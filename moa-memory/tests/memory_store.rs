//! Integration coverage for the file-backed memory store.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use moa_core::{
    ConfidenceLevel, MemoryScope, MemorySearchMode, MemoryStore, PageType, Result, WikiPage,
};
use moa_memory::FileMemoryStore;
use moa_providers::{EmbeddingProvider, MockEmbedding};
use moa_session::{PostgresSessionStore, testing};
use tempfile::{TempDir, tempdir};

fn sample_page(title: &str, page_type: PageType, content: &str) -> WikiPage {
    let timestamp = Utc.with_ymd_and_hms(2026, 4, 9, 16, 45, 0).unwrap();
    WikiPage {
        path: None,
        title: title.to_string(),
        page_type,
        content: content.to_string(),
        created: timestamp,
        updated: timestamp,
        confidence: ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: vec!["rust".to_string()],
        auto_generated: false,
        last_referenced: timestamp,
        reference_count: 5,
        metadata: std::collections::HashMap::new(),
    }
}

struct SearchHarness {
    _dir: TempDir,
    store: FileMemoryStore,
    session_store: PostgresSessionStore,
    database_url: String,
    schema_name: String,
}

impl SearchHarness {
    async fn new() -> Result<Self> {
        let dir = tempdir()?;
        let (session_store, database_url, schema_name) =
            testing::create_isolated_test_store().await?;
        let store = FileMemoryStore::new_with_pool_and_schema(
            dir.path(),
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
        )
        .await?;

        Ok(Self {
            _dir: dir,
            store,
            session_store,
            database_url,
            schema_name,
        })
    }

    async fn new_with_semantic() -> Result<Self> {
        let dir = tempdir()?;
        let (session_store, database_url, schema_name) =
            testing::create_isolated_test_store().await?;
        let store = FileMemoryStore::new_with_pool_and_schema_and_embedder(
            dir.path(),
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
            Arc::new(MockEmbedding::new(1_536)),
        )
        .await?;

        Ok(Self {
            _dir: dir,
            store,
            session_store,
            database_url,
            schema_name,
        })
    }

    async fn cleanup(self) -> Result<()> {
        let Self {
            _dir,
            store,
            session_store,
            database_url,
            schema_name,
        } = self;
        drop(store);
        drop(session_store);
        testing::cleanup_test_schema(&database_url, &schema_name).await
    }
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!("\"{}\".\"{}\"", schema_name, table_name)
}

fn workspace_scope_key(workspace_id: &str) -> String {
    format!("workspace:{workspace_id}")
}

#[derive(Clone, Default)]
struct FailingEmbedding;

#[async_trait]
impl EmbeddingProvider for FailingEmbedding {
    fn model_id(&self) -> &str {
        "failing-mock"
    }

    fn dimensions(&self) -> usize {
        1_536
    }

    async fn embed(&self, _inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(moa_core::MoaError::ProviderError(
            "simulated embedding failure".to_string(),
        ))
    }
}

#[tokio::test]
async fn create_read_update_and_delete_wiki_pages() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/authentication.md".into();

    let page = sample_page(
        "Authentication Architecture",
        PageType::Topic,
        "# Authentication Architecture\n\nThe auth system uses JWT.\n",
    );
    store.write_page(&scope, &path, page.clone()).await?;

    let loaded = store.read_page(&scope, &path).await?;
    assert_eq!(loaded.title, page.title);
    assert_eq!(loaded.page_type, PageType::Topic);
    assert!(loaded.content.contains("JWT"));

    let mut updated = loaded.clone();
    updated
        .content
        .push_str("\nRefresh tokens rotate on every use.\n");
    store.write_page(&scope, &path, updated.clone()).await?;

    let reloaded = store.read_page(&scope, &path).await?;
    assert!(reloaded.content.contains("rotate on every use"));

    store.delete_page(&scope, &path).await?;
    assert!(store.read_page(&scope, &path).await.is_err());

    Ok(())
}

#[tokio::test]
async fn fts_search_finds_ranked_results() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    for index in 0..10 {
        let title = format!("Page {index}");
        let content = if index == 0 {
            "# OAuth Refresh\n\nFix the OAuth refresh token bug in the auth service.\n"
        } else {
            "# Notes\n\nGeneric content unrelated to authentication.\n"
        };
        store
            .write_page(
                &scope,
                &format!("topics/page-{index}.md").into(),
                sample_page(&title, PageType::Topic, content),
            )
            .await?;
    }

    let results = store.search("OAuth refresh", &scope, 5).await?;
    assert!(!results.is_empty());
    assert!(results[0].snippet.contains("OAuth") || results[0].title.contains("OAuth"));
    assert_eq!(results[0].path.as_str(), "topics/page-0.md");

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn fts_search_handles_hyphenated_queries() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            &scope,
            &"skills/oauth-refresh/SKILL.md".into(),
            sample_page(
                "OAuth Refresh",
                PageType::Skill,
                "# OAuth Refresh\n\nDebug the refresh-token rotation failure.\n",
            ),
        )
        .await?;

    let results = store.search("refresh-token", &scope, 5).await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path.as_str(), "skills/oauth-refresh/SKILL.md");

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn rebuild_search_index_from_files_restores_results() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            &scope,
            &"entities/auth-service.md".into(),
            sample_page(
                "Auth Service",
                PageType::Entity,
                "# Auth Service\n\nHandles OAuth refresh token validation.\n",
            ),
        )
        .await?;

    sqlx::query(&format!(
        "INSERT INTO {} \
         (scope, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        qualified(&harness.schema_name, "wiki_pages")
    ))
    .bind(workspace_scope_key("ws1"))
    .bind("topics/stale.md")
    .bind("Stale")
    .bind("topic")
    .bind("low")
    .bind(Utc::now())
    .bind(Utc::now())
    .bind(Utc::now())
    .bind(0_i32)
    .bind(vec!["stale".to_string()])
    .bind("stale canary text")
    .execute(harness.session_store.pool())
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;

    store.rebuild_search_index(&scope).await?;
    let results = store.search("refresh token", &scope, 5).await?;
    let stale = store.search("stale canary", &scope, 5).await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path.as_str(), "entities/auth-service.md");
    assert!(stale.is_empty());

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn user_and_workspace_scopes_are_separate() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let user_scope = MemoryScope::User("u1".into());
    let workspace_scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/preferences.md".into();

    store
        .write_page(
            &user_scope,
            &path,
            sample_page(
                "Preferences",
                PageType::Topic,
                "# Preferences\n\nUser prefers concise answers.\n",
            ),
        )
        .await?;
    store
        .write_page(
            &workspace_scope,
            &path,
            sample_page(
                "Preferences",
                PageType::Topic,
                "# Preferences\n\nWorkspace requires exhaustive release notes.\n",
            ),
        )
        .await?;

    let user_page = store.read_page(&user_scope, &path).await?;
    let workspace_page = store.read_page(&workspace_scope, &path).await?;
    assert!(user_page.content.contains("concise"));
    assert!(workspace_page.content.contains("release notes"));

    let user_results = store.search("concise", &user_scope, 5).await?;
    let workspace_results = store.search("concise", &workspace_scope, 5).await?;
    assert_eq!(user_results.len(), 1);
    assert!(workspace_results.is_empty());

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn trigram_fallback_recovers_short_typos() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            &scope,
            &"topics/oauth.md".into(),
            sample_page(
                "OAuth Refresh",
                PageType::Topic,
                "# OAuth Refresh\n\nRefresh tokens rotate every 24 hours.\n",
            ),
        )
        .await?;

    let results = store.search("oatuh", &scope, 5).await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path.as_str(), "topics/oauth.md");

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn recent_pages_outrank_equally_relevant_old_pages() -> Result<()> {
    let harness = SearchHarness::new().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    let mut recent = sample_page(
        "Recent Rotation",
        PageType::Topic,
        "# Recent Rotation\n\nOAuth refresh tokens rotate every 24 hours.\n",
    );
    recent.updated = Utc::now();
    recent.last_referenced = Utc::now();

    let mut old = sample_page(
        "Old Rotation",
        PageType::Topic,
        "# Old Rotation\n\nOAuth refresh tokens rotate every 24 hours.\n",
    );
    old.updated = Utc.with_ymd_and_hms(2024, 4, 9, 16, 45, 0).unwrap();
    old.last_referenced = old.updated;

    store
        .write_page(&scope, &"topics/recent.md".into(), recent)
        .await?;
    store
        .write_page(&scope, &"topics/old.md".into(), old)
        .await?;

    let results = store.search("OAuth refresh rotation", &scope, 5).await?;

    assert!(!results.is_empty());
    assert_eq!(results[0].path.as_str(), "topics/recent.md");

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn hybrid_search_finds_semantic_matches_after_queue_drain() -> Result<()> {
    let harness = SearchHarness::new_with_semantic().await?;
    let store = &harness.store;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            &scope,
            &"topics/oauth.md".into(),
            sample_page(
                "OAuth Refresh",
                PageType::Topic,
                "# OAuth Refresh\n\nAuthentication tokens rotate during the refresh flow.\n",
            ),
        )
        .await?;
    store
        .write_page(
            &scope,
            &"topics/cache.md".into(),
            sample_page(
                "Cache Reuse",
                PageType::Topic,
                "# Cache Reuse\n\nCache entries are compacted after replay.\n",
            ),
        )
        .await?;

    assert_eq!(store.run_embedding_queue_once().await?, 2);

    let keyword = store
        .search_with_mode("identity rotation", &scope, 5, MemorySearchMode::Keyword)
        .await?;
    let semantic = store
        .search_with_mode("identity rotation", &scope, 5, MemorySearchMode::Semantic)
        .await?;
    let hybrid = store.search("identity rotation", &scope, 5).await?;

    assert!(keyword.is_empty());
    assert_eq!(semantic[0].path.as_str(), "topics/oauth.md");
    assert_eq!(hybrid[0].path.as_str(), "topics/oauth.md");

    harness.cleanup().await?;
    Ok(())
}

#[tokio::test]
async fn embedding_failures_keep_keyword_search_and_stop_after_five_attempts() -> Result<()> {
    let dir = tempdir()?;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let store = FileMemoryStore::new_with_pool_and_schema_and_embedder(
        dir.path(),
        Arc::new(session_store.pool().clone()),
        Some(&schema_name),
        Arc::new(FailingEmbedding),
    )
    .await?;
    let scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/oauth.md".into();

    store
        .write_page(
            &scope,
            &path,
            sample_page(
                "OAuth Refresh",
                PageType::Topic,
                "# OAuth Refresh\n\nRefresh tokens rotate every 24 hours.\n",
            ),
        )
        .await?;

    let keyword_results = store.search("OAuth refresh", &scope, 5).await?;
    assert_eq!(keyword_results[0].path.as_str(), "topics/oauth.md");

    for attempt in 1..=5_i32 {
        let error = store
            .run_embedding_queue_once()
            .await
            .expect_err("embedding batch should fail");
        assert!(error.to_string().contains("simulated embedding failure"));

        let recorded_attempt = sqlx::query_scalar::<_, i32>(&format!(
            "SELECT attempt_count FROM {} WHERE scope = $1 AND path = $2",
            qualified(&schema_name, "wiki_embedding_queue")
        ))
        .bind(workspace_scope_key("ws1"))
        .bind(path.as_str())
        .fetch_one(session_store.pool())
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
        assert_eq!(recorded_attempt, attempt);
    }

    assert_eq!(store.run_embedding_queue_once().await?, 0);
    assert_eq!(
        store.search("OAuth refresh", &scope, 5).await?[0].path,
        path
    );

    drop(store);
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

#[tokio::test]
async fn get_index_truncates_memory_md_to_200_lines() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let scope = MemoryScope::Workspace("ws1".into());
    let content = (0..220)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let index_path = dir
        .path()
        .join("workspaces")
        .join("ws1")
        .join("memory")
        .join("MEMORY.md");
    tokio::fs::create_dir_all(index_path.parent().unwrap()).await?;
    tokio::fs::write(index_path, content).await?;

    let loaded = store.get_index(&scope).await?;

    assert_eq!(loaded.lines().count(), 200);
    assert!(loaded.contains("line 199"));
    assert!(!loaded.contains("line 200"));

    Ok(())
}

#[tokio::test]
async fn write_page_creates_and_reads_pages_in_explicit_scopes() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let user_scope = MemoryScope::User("u1".into());
    let workspace_scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/shared.md".into();

    store
        .write_page(
            &user_scope,
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nUser scope.\n"),
        )
        .await?;
    store
        .write_page(
            &workspace_scope,
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nWorkspace scope.\n"),
        )
        .await?;

    let user_page = store.read_page(&user_scope, &path).await?;
    let workspace_page = store.read_page(&workspace_scope, &path).await?;
    assert!(user_page.content.contains("User scope."));
    assert!(workspace_page.content.contains("Workspace scope."));

    Ok(())
}

#[tokio::test]
async fn delete_page_removes_only_the_requested_scope() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let user_scope = MemoryScope::User("u1".into());
    let workspace_scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/shared.md".into();

    store
        .write_page(
            &user_scope,
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nUser scope.\n"),
        )
        .await?;
    store
        .write_page(
            &workspace_scope,
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nWorkspace scope.\n"),
        )
        .await?;

    store.delete_page(&user_scope, &path).await?;

    assert!(store.read_page(&user_scope, &path).await.is_err());
    assert!(
        store
            .read_page(&workspace_scope, &path)
            .await?
            .content
            .contains("Workspace scope.")
    );

    Ok(())
}
