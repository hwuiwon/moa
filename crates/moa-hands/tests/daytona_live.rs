// No offline counterpart possible because: this live file verifies real Daytona sandbox provisioning, lifecycle, and proxy execution semantics that a local HTTP mock cannot emulate.

//! Live Daytona integration tests.
//!
//! These tests are ignored by default because they provision real Daytona
//! sandboxes and require valid credentials in the environment.

use std::time::{Duration, Instant};
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use futures_util::FutureExt;
use moa_config::CloudHandsConfig;
use moa_config::MoaConfig;
use moa_core::{
    error::MoaError,
    error::Result,
    traits::{HandProvider, Identity, IdentityType},
    types::completion::ToolInvocation,
    types::hands::HandHandle,
    types::hands::HandResources,
    types::hands::HandSpec,
    types::hands::HandStatus,
    types::identifiers::TenantId,
    types::session::SessionMeta,
};
use moa_hands::{DaytonaHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
use tokio::time::sleep;
use uuid::Uuid;

fn session(_label: &str) -> SessionMeta {
    let identity = identity();
    SessionMeta {
        tenant_id: identity.tenant_id,
        model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c331),
        tenant_id: TenantId::from(Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c332)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn live_provider() -> DaytonaHandProvider {
    let api_key = std::env::var("DAYTONA_API_KEY").expect("DAYTONA_API_KEY must be set");
    let api_url = std::env::var("DAYTONA_API_URL")
        .unwrap_or_else(|_| "https://app.daytona.io/api".to_string());
    DaytonaHandProvider::with_urls(api_key, api_url, "https://proxy.app.daytona.io/toolbox")
        .expect("failed to build Daytona provider")
}

fn live_daytona_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_DAYTONA_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_daytona_credentials() {
    assert!(
        std::env::var("DAYTONA_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        "MOA_RUN_LIVE_DAYTONA_TESTS=1 requires DAYTONA_API_KEY"
    );
}

fn live_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("daytona".to_string()),
        daytona_api_key: Some(std::env::var("DAYTONA_API_KEY").expect("DAYTONA_API_KEY")),
        daytona_api_url: Some(
            std::env::var("DAYTONA_API_URL")
                .unwrap_or_else(|_| "https://app.daytona.io/api".to_string()),
        ),
        ..CloudHandsConfig::default()
    });
    config
}

async fn wait_for_destroyed(
    provider: &DaytonaHandProvider,
    handle: &HandHandle,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(
                "timed out waiting for Daytona sandbox destruction".to_string(),
            ));
        }
        if matches!(provider.status(handle).await?, HandStatus::Destroyed) {
            return Ok(());
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn wait_for_status(
    provider: &DaytonaHandProvider,
    handle: &HandHandle,
    expected: &[HandStatus],
    timeout: Duration,
) -> Result<HandStatus> {
    let started = Instant::now();
    loop {
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(format!(
                "timed out waiting for Daytona status {expected:?}"
            )));
        }
        let status = provider.status(handle).await?;
        if expected.contains(&status) {
            return Ok(status);
        }
        sleep(Duration::from_secs(2)).await;
    }
}

async fn destroy_and_wait(provider: &DaytonaHandProvider, handle: &HandHandle) -> Result<()> {
    provider.destroy(handle).await?;
    wait_for_destroyed(provider, handle, Duration::from_secs(30)).await
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1 and DAYTONA_API_KEY"]
async fn daytona_provider_round_trip() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let provider = live_provider();

    let unsupported = provider
        .provision(HandSpec {
            sandbox_tier: moa_core::types::hands::SandboxTier::MicroVM,
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
            sandbox_tier: moa_core::types::hands::SandboxTier::Container,
            image: None,
            resources: HandResources::default(),
            env: std::collections::HashMap::new(),
            workspace_mount: None,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(600),
        })
        .await
        .expect("failed to provision Daytona sandbox");

    let file_path = format!("tmp/moa-daytona-live-{}.txt", Uuid::now_v7().simple());
    let marker = format!("marker-{}", Uuid::now_v7().simple());

    let result = AssertUnwindSafe(async {
        let status = provider.status(&handle).await?;
        assert!(
            matches!(
                status,
                HandStatus::Provisioning
                    | HandStatus::Running
                    | HandStatus::Stopped
                    | HandStatus::Paused
            ),
            "unexpected initial status: {status:?}"
        );

        let bash = provider
            .execute(
                &handle,
                "bash",
                &json!({
                    "cmd": format!("sh -lc 'printf {marker}'"),
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

        let write = provider
            .execute(
                &handle,
                "file_write",
                &json!({ "path": file_path, "content": marker }).to_string(),
            )
            .await?;
        assert_eq!(
            write.to_text(),
            format!("[new file created: {file_path}, 1 lines]")
        );

        let read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert_eq!(read.to_text(), marker);

        let search = provider
            .execute(
                &handle,
                "file_search",
                &json!({ "pattern": file_path.rsplit('/').next().unwrap_or_default() }).to_string(),
            )
            .await?;
        assert!(!search.is_error);
        assert!(
            search.to_text().contains(&file_path),
            "search output missing path: {}",
            search.to_text()
        );

        provider.pause(&handle).await?;
        let _ = wait_for_status(
            &provider,
            &handle,
            &[HandStatus::Stopped, HandStatus::Paused],
            Duration::from_secs(60),
        )
        .await?;
        let resumed_read = provider
            .execute(
                &handle,
                "file_read",
                &json!({ "path": file_path }).to_string(),
            )
            .await?;
        assert_eq!(resumed_read.to_text(), marker);

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
            panic!("live Daytona provider test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1 and DAYTONA_API_KEY"]
async fn daytona_router_reuses_and_isolates() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let mut config = live_config();
    let temp = tempdir().expect("tempdir");
    config.local.sandbox_dir = temp.path().join("sandbox").display().to_string();

    let router = ToolRouter::from_config(&config, None, None)
        .await
        .expect("router should load Daytona from config");
    let provider = DaytonaHandProvider::from_config(&config).expect("provider from config");

    let session_one = session("one");
    let session_two = session("two");
    let file_one = format!("tmp/moa-router-one-{}.txt", Uuid::now_v7().simple());
    let file_two = format!("tmp/moa-router-two-{}.txt", Uuid::now_v7().simple());
    let content_one = format!("router-one-{}", Uuid::now_v7().simple());
    let content_two = format!("router-two-{}", Uuid::now_v7().simple());

    let handle_one_id = {
        let (hand_id, write) = router
            .execute_authorized(
                &session_one,
                &identity(),
                &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_one, "content": content_one }),
                },
            )
            .await
            .expect("first router write should provision a hand");
        assert_eq!(
            write.to_text(),
            format!("[new file created: {file_one}, 1 lines]")
        );
        hand_id.expect("cloud hand execution should return a hand id")
    };

    let handle_one = HandHandle::daytona(handle_one_id.clone());
    let mut handle_two: Option<HandHandle> = None;
    let test_result = AssertUnwindSafe(async {
        let (same_hand_id, read) = router
            .execute_authorized(
                &session_one,
                &identity(),
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
            )
            .await?;
        assert_eq!(same_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert_eq!(read.to_text(), content_one);

        provider.pause(&handle_one).await?;
        let _ = wait_for_status(
            &provider,
            &handle_one,
            &[HandStatus::Stopped, HandStatus::Paused],
            Duration::from_secs(60),
        )
        .await?;
        let (resumed_hand_id, resumed_read) = router
            .execute_authorized(
                &session_one,
                &identity(),
                &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
            )
            .await?;
        assert_eq!(resumed_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert_eq!(resumed_read.to_text(), content_one);

        let (hand_two_id, second_write) = router
            .execute_authorized(
                &session_two,
                &identity(),
                &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_two, "content": content_two }),
                },
            )
            .await?;
        assert_eq!(
            second_write.to_text(),
            format!("[new file created: {file_two}, 1 lines]")
        );
        let hand_two_id = hand_two_id.expect("second session should receive a distinct hand");
        assert_ne!(hand_two_id, handle_one_id);
        handle_two = Some(HandHandle::daytona(hand_two_id.clone()));

        let (_, bash) = router
            .execute_authorized(
                &session_two,
                &identity(),
                &ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": "sh -lc 'printf router-bash'", "timeout_secs": 60 }),
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
            panic!("live Daytona router test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("router cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}
