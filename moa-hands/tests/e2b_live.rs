//! Live E2B integration tests.
//!
//! These tests are ignored by default because they provision real E2B sandboxes
//! and require valid credentials in the environment.

#![cfg(feature = "e2b")]

use std::sync::Arc;
use std::time::Duration;
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use async_trait::async_trait;
use futures_util::FutureExt;
use moa_core::{
    CloudHandsConfig, HandHandle, HandProvider, HandResources, HandSpec, HandStatus, MemoryPath,
    MemoryScope, MemorySearchResult, MemoryStore, MoaConfig, MoaError, PageSummary, PageType,
    Result, SessionMeta, ToolInvocation, UserId, WikiPage, WorkspaceId,
};
use moa_hands::{E2BHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

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
        Err(MoaError::StorageError("not found".to_string()))
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

fn session(label: &str) -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new("e2b-live-workspace"),
        user_id: UserId::new(format!("e2b-live-user-{label}")),
        model: "claude-sonnet-4-6".to_string(),
        ..SessionMeta::default()
    }
}

fn live_provider() -> E2BHandProvider {
    let api_key = std::env::var("E2B_API_KEY").expect("E2B_API_KEY must be set");
    let api_url =
        std::env::var("E2B_API_URL").unwrap_or_else(|_| "https://api.e2b.dev".to_string());
    let domain = std::env::var("E2B_DOMAIN").unwrap_or_else(|_| "e2b.app".to_string());
    let template = std::env::var("E2B_TEMPLATE").unwrap_or_else(|_| "base".to_string());
    E2BHandProvider::with_api_url(api_key, api_url, domain, template)
        .expect("failed to build E2B provider")
}

fn live_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.cloud.enabled = true;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        e2b_api_key_env: Some("E2B_API_KEY".to_string()),
        e2b_api_url: Some(
            std::env::var("E2B_API_URL").unwrap_or_else(|_| "https://api.e2b.dev".to_string()),
        ),
        e2b_domain: Some(std::env::var("E2B_DOMAIN").unwrap_or_else(|_| "e2b.app".to_string())),
        e2b_template: Some(std::env::var("E2B_TEMPLATE").unwrap_or_else(|_| "base".to_string())),
        ..CloudHandsConfig::default()
    });
    config
}

async fn wait_for_destroyed(
    provider: &E2BHandProvider,
    handle: &HandHandle,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(
                "timed out waiting for E2B sandbox destruction".to_string(),
            ));
        }
        if matches!(provider.status(handle).await?, HandStatus::Destroyed) {
            return Ok(());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn destroy_and_wait(provider: &E2BHandProvider, handle: &HandHandle) -> Result<()> {
    provider.destroy(handle).await?;
    wait_for_destroyed(provider, handle, Duration::from_secs(30)).await
}

#[tokio::test]
#[ignore = "manual live E2B test"]
async fn e2b_live_provider_handles_roundtrip_and_lifecycle() {
    let provider = live_provider();

    let unsupported = provider
        .provision(HandSpec {
            sandbox_tier: moa_core::SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(600),
        })
        .await;
    assert!(matches!(unsupported, Err(MoaError::Unsupported(_))));

    let handle = provider
        .provision(HandSpec {
            sandbox_tier: moa_core::SandboxTier::MicroVM,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(600),
        })
        .await
        .expect("failed to provision E2B sandbox");

    let file_path = format!("/tmp/moa-e2b-live-{}.txt", Uuid::new_v4().simple());
    let marker = format!("marker-{}", Uuid::new_v4().simple());

    let result = AssertUnwindSafe(async {
        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": format!("printf {marker}"),
                    "timeout_secs": 60_u64
                })
                .to_string(),
            )
            .await?;
        assert_eq!(
            bash.process_exit_code(),
            Some(0),
            "bash stderr: {}",
            bash.process_stderr().unwrap_or_default()
        );
        assert!(
            bash.process_stdout().unwrap_or_default().contains(&marker),
            "bash output missing marker: {}",
            bash.to_text()
        );

        let failing = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": "printf live-out; printf live-err >&2; exit 7",
                    "timeout_secs": 60_u64
                })
                .to_string(),
            )
            .await?;
        assert_eq!(failing.process_exit_code(), Some(7));
        assert!(
            failing
                .process_stdout()
                .unwrap_or_default()
                .contains("live-out")
        );
        assert!(
            failing
                .process_stderr()
                .unwrap_or_default()
                .contains("live-err")
        );

        let write = provider
            .execute(
                &handle,
                "file_write",
                &json!({ "path": file_path, "content": marker }).to_string(),
            )
            .await?;
        assert_eq!(write.process_exit_code(), Some(0));

        let read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert!(
            read.to_text().contains(&marker),
            "read output missing marker: {}",
            read.to_text()
        );

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": file_path.rsplit('/').next().unwrap_or_default() }).to_string(),
            )
            .await?;
        assert_eq!(search.process_exit_code(), Some(0));
        assert!(
            search.to_text().contains(&file_path)
                || search
                    .to_text()
                    .contains(file_path.rsplit('/').next().unwrap_or_default()),
            "search output missing path: {}",
            search.to_text()
        );

        provider.pause(&handle).await?;
        provider.resume(&handle).await?;
        let resumed_read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert!(resumed_read.to_text().contains(&marker));

        let unsupported_tool = provider
            .execute(
                &handle,
                "web_search",
                &json!({ "query": "test" }).to_string(),
            )
            .await;
        assert!(matches!(unsupported_tool, Err(MoaError::ToolError(_))));

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = destroy_and_wait(&provider, &handle).await;
    let second_destroy = provider.destroy(&handle).await;

    match result {
        Ok(Ok(())) => {
            cleanup_result.expect("sandbox cleanup should succeed");
            assert!(
                second_destroy.is_ok(),
                "destroy should be idempotent, got: {second_destroy:?}"
            );
        }
        Ok(Err(error)) => {
            cleanup_result.expect("sandbox cleanup should succeed after provider failure");
            panic!("live E2B provider test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "manual live E2B test"]
async fn e2b_live_router_lazy_provisions_reuses_and_isolates_sessions() {
    let mut config = live_config();
    let temp = tempdir().expect("tempdir");
    config.local.sandbox_dir = temp.path().join("sandbox").display().to_string();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);

    let router = ToolRouter::from_config(&config, memory_store)
        .await
        .expect("router should load E2B from config");
    let provider = E2BHandProvider::from_config(&config).expect("provider from config");

    let session_one = session("one");
    let session_two = session("two");
    let file_one = format!("/tmp/moa-e2b-router-one-{}.txt", Uuid::new_v4().simple());
    let file_two = format!("/tmp/moa-e2b-router-two-{}.txt", Uuid::new_v4().simple());
    let content_one = format!("router-one-{}", Uuid::new_v4().simple());
    let content_two = format!("router-two-{}", Uuid::new_v4().simple());

    let handle_one_id = {
        let (hand_id, write) = router
            .execute_authorized(
                &session_one,
                &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_one, "content": content_one }),
                },
            )
            .await
            .expect("first router write should provision a hand");
        assert_eq!(write.process_exit_code(), Some(0));
        hand_id.expect("cloud hand execution should return a hand id")
    };

    let handle_one = HandHandle::e2b(handle_one_id.clone());
    let mut handle_two: Option<HandHandle> = None;
    let test_result = AssertUnwindSafe(async {
        let (same_hand_id, read) = router
            .execute_authorized(
                &session_one,
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
            )
            .await?;
        assert_eq!(same_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert!(read.to_text().contains(&content_one));

        provider.pause(&handle_one).await?;
        let (resumed_hand_id, resumed_read) = router
            .execute_authorized(
                &session_one,
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
            )
            .await?;
        assert_eq!(resumed_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert!(resumed_read.to_text().contains(&content_one));

        let (hand_two_id, second_write) = router
            .execute_authorized(
                &session_two,
                &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_two, "content": content_two }),
                },
            )
            .await?;
        assert_eq!(second_write.process_exit_code(), Some(0));
        let hand_two_id = hand_two_id.expect("second session should receive a distinct hand");
        assert_ne!(hand_two_id, handle_one_id);
        handle_two = Some(HandHandle::e2b(hand_two_id.clone()));

        let missing_read = router
            .execute_authorized(
                &session_two,
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
            )
            .await;
        match missing_read {
            Ok((_, output)) => {
                assert_ne!(
                    output.process_exit_code(),
                    Some(0),
                    "second sandbox unexpectedly read first sandbox file: {}",
                    output.to_text()
                );
            }
            Err(error) => match error {
                MoaError::HttpStatus { status, .. } => assert_eq!(status, 404),
                other => panic!("unexpected second-sandbox read failure: {other}"),
            },
        }

        let (_, bash) = router
            .execute_authorized(
                &session_two,
                &ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": "printf router-bash", "timeout_secs": 60 }),
                },
            )
            .await?;
        assert_eq!(bash.process_exit_code(), Some(0));
        assert!(bash.to_text().contains("router-bash"));

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = async {
        if let Some(handle_two) = &handle_two {
            destroy_and_wait(&provider, handle_two).await?;
        }
        destroy_and_wait(&provider, &handle_one).await
    }
    .await;

    match test_result {
        Ok(Ok(())) => cleanup_result.expect("router cleanup should succeed"),
        Ok(Err(error)) => {
            cleanup_result.expect("router cleanup should succeed after provider failure");
            panic!("live E2B router test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("router cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}
