use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    Event, HandProvider, HandResources, HandSpec, MemoryPath, MemoryScope, MemorySearchResult,
    MemoryStore, PageSummary, PageType, Result, SandboxTier, SessionMeta, SessionStore,
    ToolInvocation, UserId, WikiPage, WorkspaceId,
};
use moa_hands::{LocalHandProvider, ToolRouter};
use moa_memory::FileMemoryStore;
use moa_session::TursoSessionStore;
use serde_json::json;
use tempfile::tempdir;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

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

    async fn read_page(&self, _scope: MemoryScope, _path: &MemoryPath) -> Result<WikiPage> {
        Err(moa_core::MoaError::StorageError("not found".to_string()))
    }

    async fn write_page(
        &self,
        _scope: MemoryScope,
        _path: &MemoryPath,
        _page: WikiPage,
    ) -> Result<()> {
        Ok(())
    }

    async fn delete_page(&self, _scope: MemoryScope, _path: &MemoryPath) -> Result<()> {
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

fn sample_page(path: &str, title: &str, page_type: PageType, content: &str) -> WikiPage {
    WikiPage {
        path: Some(MemoryPath::new(path)),
        title: title.to_string(),
        page_type,
        content: content.to_string(),
        created: chrono::Utc::now(),
        updated: chrono::Utc::now(),
        confidence: moa_core::ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: Vec::new(),
        auto_generated: false,
        last_referenced: chrono::Utc::now(),
        reference_count: 1,
        metadata: std::collections::HashMap::new(),
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
        .execute_authorized(
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
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "notes.txt" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.to_text(), "hello");
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
        .execute_authorized(
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
        .execute_authorized(
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
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_search".to_string(),
                input: json!({ "pattern": "**/*.rs" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("src/lib.rs"));
    assert!(!rendered.contains("notes.txt"));
}

#[tokio::test]
async fn default_router_excludes_provider_native_web_tools() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap();

    assert!(!router.has_tool("web_search"));
    assert!(!router.has_tool("web_fetch"));
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
        .execute_authorized(
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
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "bash".to_string(),
                input: json!({ "cmd": "printf 'out'; printf 'err' 1>&2" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.process_stdout(), Some("out"));
    assert_eq!(output.process_stderr(), Some("err"));
    assert_eq!(output.process_exit_code(), Some(0));
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
        .execute_authorized(
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
async fn session_search_finds_prior_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let session_store = Arc::new(TursoSessionStore::new_local(&db_path).await.unwrap());
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: "deploy failed on port binding".to_string(),
                attachments: vec![],
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "I found the deploy failure".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 10,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "port binding", "last_n": 3 }),
            },
        )
        .await
        .unwrap();

    assert!(output.to_text().contains("deploy failed on port binding"));
    assert!(
        output
            .structured
            .as_ref()
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty())
    );
}

#[tokio::test]
async fn session_search_filters_error_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("sessions.db");
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let session_store = Arc::new(TursoSessionStore::new_local(&db_path).await.unwrap());
    let router = ToolRouter::new_local(memory_store, dir.path())
        .await
        .unwrap()
        .with_session_store(session_store.clone());
    let session = session();
    let session_id = session_store.create_session(session.clone()).await.unwrap();

    session_store
        .emit_event(
            session_id.clone(),
            Event::Error {
                message: "deploy error".to_string(),
                recoverable: true,
            },
        )
        .await
        .unwrap();
    session_store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "deploy completed successfully".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                input_tokens: 10,
                output_tokens: 5,
                cost_cents: 1,
                duration_ms: 20,
            },
        )
        .await
        .unwrap();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "session_search".to_string(),
                input: json!({ "query": "deploy", "event_type": "error" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("deploy error"));
    assert!(!rendered.contains("deploy completed successfully"));
}

#[tokio::test]
async fn local_bash_hard_cancel_kills_running_process() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
    let router = Arc::new(
        ToolRouter::new_local(memory_store, dir.path())
            .await
            .unwrap(),
    );
    let session = session();
    let cancel_token = CancellationToken::new();
    let started = Instant::now();
    let invocation = ToolInvocation {
        id: None,
        name: "bash".to_string(),
        input: json!({ "cmd": "python3 -c 'import time; time.sleep(60)'" }),
    };

    let task = {
        let router = router.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            router
                .execute_authorized_with_cancel(&session, &invocation, None, Some(&cancel_token))
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel_token.cancel();

    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, moa_core::MoaError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn memory_search_returns_indexed_results() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    memory_store
        .write_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
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
                metadata: std::collections::HashMap::new(),
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

    let rendered = output.to_text();
    assert!(rendered.contains("OAuth Notes"));
    assert!(rendered.contains("refresh"));
    assert!(output.structured.is_some());
}

#[tokio::test]
async fn memory_read_returns_page_contents() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    memory_store
        .write_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &MemoryPath::new("skills/oauth-refresh/SKILL.md"),
            WikiPage {
                path: Some(MemoryPath::new("skills/oauth-refresh/SKILL.md")),
                title: "OAuth Refresh".to_string(),
                page_type: PageType::Skill,
                content: "# OAuth Refresh\nUse the exact workflow.".to_string(),
                created: chrono::Utc::now(),
                updated: chrono::Utc::now(),
                confidence: moa_core::ConfidenceLevel::High,
                related: Vec::new(),
                sources: Vec::new(),
                tags: vec!["auth".to_string()],
                auto_generated: false,
                last_referenced: chrono::Utc::now(),
                reference_count: 1,
                metadata: std::collections::HashMap::new(),
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
                name: "memory_read".to_string(),
                input: json!({ "path": "skills/oauth-refresh/SKILL.md" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("# OAuth Refresh"));
    assert!(rendered.contains("Use the exact workflow."));
}

#[tokio::test]
async fn memory_write_with_scope_creates_new_workspace_page() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    let memory_store_trait: Arc<dyn MemoryStore> = memory_store.clone();
    let router = ToolRouter::new_local(memory_store_trait, dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();

    let (_, output) = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "memory_write".to_string(),
                input: json!({
                    "path": "topics/new-page.md",
                    "scope": "workspace",
                    "title": "New Page",
                    "content": "# New Page\nCreated from the tool."
                }),
            },
        )
        .await
        .unwrap();

    assert!(
        output
            .to_text()
            .contains("Wrote memory page topics/new-page.md")
    );
    let page = memory_store
        .read_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &MemoryPath::new("topics/new-page.md"),
        )
        .await
        .unwrap();
    assert_eq!(page.title, "New Page");
    assert!(page.content.contains("Created from the tool."));
}

#[tokio::test]
async fn memory_write_without_scope_updates_existing_page() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    memory_store
        .write_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &MemoryPath::new("topics/existing.md"),
            sample_page(
                "topics/existing.md",
                "Existing",
                PageType::Topic,
                "# Existing\nBefore.",
            ),
        )
        .await
        .unwrap();
    let memory_store_trait: Arc<dyn MemoryStore> = memory_store.clone();
    let router = ToolRouter::new_local(memory_store_trait, dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "memory_write".to_string(),
                input: json!({
                    "path": "topics/existing.md",
                    "content": "# Existing\nAfter."
                }),
            },
        )
        .await
        .unwrap();

    let page = memory_store
        .read_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &MemoryPath::new("topics/existing.md"),
        )
        .await
        .unwrap();
    assert!(page.content.contains("After."));
}

#[tokio::test]
async fn memory_write_without_scope_requires_scope_for_new_page() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    let memory_store_trait: Arc<dyn MemoryStore> = memory_store;
    let router = ToolRouter::new_local(memory_store_trait, dir.path().join("sandboxes"))
        .await
        .unwrap();
    let session = session();

    let error = router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "memory_write".to_string(),
                input: json!({
                    "path": "topics/new-page.md",
                    "content": "# New Page\nNeeds scope."
                }),
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        moa_core::MoaError::ToolError(message)
            if message.contains("specify `scope`") && message.contains("topics/new-page.md")
    ));
}

#[tokio::test]
async fn memory_read_without_scope_falls_back_to_user_scope() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    memory_store
        .write_page(
            MemoryScope::User(UserId::new("user")),
            &MemoryPath::new("topics/preferences.md"),
            sample_page(
                "topics/preferences.md",
                "Preferences",
                PageType::Topic,
                "# Preferences\nUser-only page.",
            ),
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
                name: "memory_read".to_string(),
                input: json!({ "path": "topics/preferences.md" }),
            },
        )
        .await
        .unwrap();

    assert!(output.to_text().contains("User-only page."));
}

#[tokio::test]
async fn memory_read_with_explicit_scope_reads_only_that_scope() {
    let dir = tempdir().unwrap();
    let memory_root = dir.path().join("memory-root");
    let memory_store = Arc::new(FileMemoryStore::new(&memory_root).await.unwrap());
    let path = MemoryPath::new("topics/preferences.md");
    memory_store
        .write_page(
            MemoryScope::User(UserId::new("user")),
            &path,
            sample_page(
                "topics/preferences.md",
                "Preferences",
                PageType::Topic,
                "# Preferences\nUser page.",
            ),
        )
        .await
        .unwrap();
    memory_store
        .write_page(
            MemoryScope::Workspace(WorkspaceId::new("workspace")),
            &path,
            sample_page(
                "topics/preferences.md",
                "Preferences",
                PageType::Topic,
                "# Preferences\nWorkspace page.",
            ),
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
                name: "memory_read".to_string(),
                input: json!({ "path": "topics/preferences.md", "scope": "user" }),
            },
        )
        .await
        .unwrap();

    let rendered = output.to_text();
    assert!(rendered.contains("User page."));
    assert!(!rendered.contains("Workspace page."));
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn docker_file_tools_roundtrip_inside_container_workspace() {
    let dir = tempdir().unwrap();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    if !provider.docker_available() {
        return;
    }

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();

    if !matches!(handle, moa_core::HandHandle::Docker { .. }) {
        return;
    }

    let result = async {
        let write = provider
            .execute(
                &handle,
                "file_write",
                &json!({ "path": "nested/demo.txt", "content": "hello from docker file tool" })
                    .to_string(),
            )
            .await
            .unwrap();
        assert_eq!(write.to_text(), "wrote nested/demo.txt");

        let read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": "nested/demo.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert_eq!(read.to_text(), "hello from docker file tool");

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": "**/*.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert!(search.to_text().contains("nested/demo.txt"));

        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({ "cmd": "cat /workspace/nested/demo.txt" }).to_string(),
            )
            .await
            .unwrap();
        assert!(bash.to_text().contains("hello from docker file tool"));
    }
    .await;

    let _ = provider.destroy(&handle).await;
    result
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn docker_bash_hard_cancel_stops_container_exec() {
    let dir = tempdir().unwrap();
    let provider = LocalHandProvider::new(dir.path()).await.unwrap();
    if !provider.docker_available() {
        return;
    }

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(300),
        })
        .await
        .unwrap();

    if !matches!(handle, moa_core::HandHandle::Docker { .. }) {
        return;
    }

    let cancel_token = CancellationToken::new();
    let started = Instant::now();
    let task = {
        let provider = provider.clone();
        let handle = handle.clone();
        let cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            provider
                .execute_with_cancel(
                    &handle,
                    "bash",
                    &json!({ "cmd": "sleep 60" }).to_string(),
                    Some(&cancel_token),
                )
                .await
        })
    };

    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel_token.cancel();

    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, moa_core::MoaError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));

    let _ = provider.destroy(&handle).await;
}
