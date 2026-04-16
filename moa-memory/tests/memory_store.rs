//! Integration coverage for the file-backed memory store.

use chrono::{TimeZone, Utc};
use moa_core::{ConfidenceLevel, MemoryScope, MemoryStore, PageType, Result, WikiPage};
use moa_memory::FileMemoryStore;
use tempfile::tempdir;

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
    store.write_page(scope.clone(), &path, page.clone()).await?;

    let loaded = store.read_page(scope.clone(), &path).await?;
    assert_eq!(loaded.title, page.title);
    assert_eq!(loaded.page_type, PageType::Topic);
    assert!(loaded.content.contains("JWT"));

    let mut updated = loaded.clone();
    updated
        .content
        .push_str("\nRefresh tokens rotate on every use.\n");
    store
        .write_page(scope.clone(), &path, updated.clone())
        .await?;

    let reloaded = store.read_page(scope.clone(), &path).await?;
    assert!(reloaded.content.contains("rotate on every use"));

    store.delete_page(scope.clone(), &path).await?;
    assert!(store.read_page(scope, &path).await.is_err());

    Ok(())
}

#[tokio::test]
#[ignore = "search disabled until step 90 lands the Postgres tsvector index"]
async fn fts_search_finds_ranked_results() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
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
                scope.clone(),
                &format!("topics/page-{index}.md").into(),
                sample_page(&title, PageType::Topic, content),
            )
            .await?;
    }

    let results = store.search("OAuth refresh", scope, 5).await?;
    assert!(!results.is_empty());
    assert!(results[0].snippet.contains("OAuth") || results[0].title.contains("OAuth"));
    assert_eq!(results[0].path.as_str(), "topics/page-0.md");

    Ok(())
}

#[tokio::test]
#[ignore = "search disabled until step 90 lands the Postgres tsvector index"]
async fn fts_search_handles_hyphenated_queries() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            scope.clone(),
            &"skills/oauth-refresh/SKILL.md".into(),
            sample_page(
                "OAuth Refresh",
                PageType::Skill,
                "# OAuth Refresh\n\nDebug the refresh-token rotation failure.\n",
            ),
        )
        .await?;

    let results = store.search("refresh-token", scope, 5).await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path.as_str(), "skills/oauth-refresh/SKILL.md");

    Ok(())
}

#[tokio::test]
#[ignore = "search disabled until step 90 lands the Postgres tsvector index"]
async fn rebuild_search_index_from_files_restores_results() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let scope = MemoryScope::Workspace("ws1".into());

    store
        .write_page(
            scope.clone(),
            &"entities/auth-service.md".into(),
            sample_page(
                "Auth Service",
                PageType::Entity,
                "# Auth Service\n\nHandles OAuth refresh token validation.\n",
            ),
        )
        .await?;

    let rebuilt = FileMemoryStore::new(dir.path()).await?;
    rebuilt.rebuild_search_index(scope.clone()).await?;
    let results = rebuilt.search("refresh token", scope, 5).await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path.as_str(), "entities/auth-service.md");

    Ok(())
}

#[tokio::test]
#[ignore = "search disabled until step 90 lands the Postgres tsvector index"]
async fn user_and_workspace_scopes_are_separate() -> Result<()> {
    let dir = tempdir()?;
    let store = FileMemoryStore::new(dir.path()).await?;
    let user_scope = MemoryScope::User("u1".into());
    let workspace_scope = MemoryScope::Workspace("ws1".into());
    let path = "topics/preferences.md".into();

    store
        .write_page(
            user_scope.clone(),
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
            workspace_scope.clone(),
            &path,
            sample_page(
                "Preferences",
                PageType::Topic,
                "# Preferences\n\nWorkspace requires exhaustive release notes.\n",
            ),
        )
        .await?;

    let user_page = store.read_page(user_scope.clone(), &path).await?;
    let workspace_page = store.read_page(workspace_scope.clone(), &path).await?;
    assert!(user_page.content.contains("concise"));
    assert!(workspace_page.content.contains("release notes"));

    let user_results = store.search("concise", user_scope, 5).await?;
    let workspace_results = store.search("concise", workspace_scope, 5).await?;
    assert_eq!(user_results.len(), 1);
    assert!(workspace_results.is_empty());

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

    let loaded = store.get_index(scope).await?;

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
            user_scope.clone(),
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nUser scope.\n"),
        )
        .await?;
    store
        .write_page(
            workspace_scope.clone(),
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nWorkspace scope.\n"),
        )
        .await?;

    let user_page = store.read_page(user_scope, &path).await?;
    let workspace_page = store.read_page(workspace_scope, &path).await?;
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
            user_scope.clone(),
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nUser scope.\n"),
        )
        .await?;
    store
        .write_page(
            workspace_scope.clone(),
            &path,
            sample_page("Shared", PageType::Topic, "# Shared\n\nWorkspace scope.\n"),
        )
        .await?;

    store.delete_page(user_scope.clone(), &path).await?;

    assert!(store.read_page(user_scope, &path).await.is_err());
    assert!(
        store
            .read_page(workspace_scope, &path)
            .await?
            .content
            .contains("Workspace scope.")
    );

    Ok(())
}
