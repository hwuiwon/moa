use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    MemoryPath, MemoryScope, MemorySearchResult, MemoryStore, PageSummary, PageType, Result,
    SessionMeta, ToolInvocation, UserId, WikiPage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use serde_json::json;
use tempfile::tempdir;

#[derive(Default)]
struct EmptyMemoryStore;

#[async_trait]
impl MemoryStore for EmptyMemoryStore {
    async fn search(
        &self,
        _query: &str,
        _scope: MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Ok(Vec::new())
    }

    async fn read_page(&self, _path: &MemoryPath) -> Result<WikiPage> {
        Err(moa_core::MoaError::StorageError("not found".to_string()))
    }

    async fn write_page(&self, _path: &MemoryPath, _page: WikiPage) -> Result<()> {
        Ok(())
    }

    async fn delete_page(&self, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    async fn list_pages(
        &self,
        _scope: MemoryScope,
        _filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        Ok(Vec::new())
    }

    async fn get_index(&self, _scope: MemoryScope) -> Result<String> {
        Ok(String::new())
    }

    async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
        Ok(())
    }
}

fn session() -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        model: "claude-sonnet-4-6".to_string(),
        ..SessionMeta::default()
    }
}

#[tokio::test]
async fn file_read_reads_written_content() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();
    let session = session();

    router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "hello" }),
            },
        )
        .await
        .unwrap();
    let (_, output) = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "notes.txt" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.stdout, "hello");
}

#[tokio::test]
async fn file_search_finds_files_by_glob() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();
    let session = session();

    router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "src/lib.rs", "content": "pub fn demo() {}" }),
            },
        )
        .await
        .unwrap();
    router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "ignore me" }),
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*.rs" }),
            },
        )
        .await
        .unwrap();

    assert!(output.stdout.contains("src/lib.rs"));
    assert!(!output.stdout.contains("notes.txt"));
}

#[tokio::test]
async fn file_operations_reject_path_traversal() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();
    let session = session();

    let error = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "../secret.txt" }),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(error, moa_core::MoaError::PermissionDenied(_)));
}

#[tokio::test]
async fn bash_captures_stdout_and_stderr() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();
    let session = session();

    let (_, output) = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "printf 'out'; printf 'err' 1>&2" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.stdout, "out");
    assert_eq!(output.stderr, "err");
    assert_eq!(output.exit_code, 0);
}

#[tokio::test]
async fn bash_respects_timeout() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();
    let session = session();

    let error = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "sleep 10", "timeout_secs": 1 }),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, moa_core::MoaError::ToolError(message) if message.contains("timed out"))
    );
}

#[tokio::test]
async fn memory_search_returns_indexed_results() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    memory_store
        .write_page_in_scope(
            &MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &MemoryPath::new("topics/oauth.md"),
            WikiPage {
                path: Some(MemoryPath::new("topics/oauth.md")),
                title: "OAuth Notes".to_string(),
                page_type: PageType::Topic,
                content: "# OAuth Notes\nFix the refresh token bug.".to_string(),
                created: chrono::Utc::now(),
                updated: chrono::Utc::now(),
                confidence: moa_core::ConfidenceLevel::High,
                related: Vec::new(),
                sources: Vec::new(),
                tags: vec!["auth".to_string()],
                auto_generated: false,
                last_referenced: chrono::Utc::now(),
                reference_count: 1,
            },
        )
        .await
        .unwrap();

    let memory_store_trait: Arc<dyn MemoryStore> = memory_store;
    let router = ToolRouter::new_local(memory_store_trait, dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();
    let (_, output) = router
        .execute(
            &session,
            &ToolInvocation {
                id: None,
                name: "memory_search".to_string(),
                input: json!({ "query": "refresh token", "scope": "workspace" }),
            },
        )
        .await
        .unwrap();

    assert!(output.stdout.contains("OAuth Notes"));
    assert!(output.stdout.contains("refresh"));
}
