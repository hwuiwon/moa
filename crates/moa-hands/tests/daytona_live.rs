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
use moa_core::types::action_policy::CallOrigin;
use moa_core::types::identifiers::ToolCallId;
use moa_core::{
    error::MoaError,
    error::Result,
    traits::{HandProvider, Identity, IdentityType},
    types::completion::ToolInvocation,
    types::hands::HandHandle,
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

/// Waits until a durable provisioning operation resolves to no live sandbox.
///
/// The list API is only bounded-consistent after a destroy, so a destroyed
/// sandbox is allowed to linger in the label-filtered listing briefly.
async fn wait_for_no_provisioned_hands(
    provider: &DaytonaHandProvider,
    operation_id: moa_core::types::identifiers::HandProvisioningOperationId,
    timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let discovered = provider.provisioned_hands(operation_id).await?;
        if discovered.is_empty() {
            return Ok(());
        }
        if started.elapsed() > timeout {
            return Err(MoaError::ProviderError(format!(
                "durable provisioning operation `{operation_id}` still resolves to {discovered:?}"
            )));
        }
        sleep(Duration::from_secs(2)).await;
    }
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
        .provision(live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM))
        .await;
    assert!(matches!(unsupported, Err(MoaError::Unsupported(_))));

    let handle = provider
        .provision(live_hand_spec(
            moa_core::types::hands::SandboxTier::Container,
        ))
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

// Pins: a live sandbox created under a durable provisioning operation ID is
// discoverable by that ID through Daytona's real label-filtered list API;
// re-provisioning the same operation resolves to the same sandbox by its
// deterministic name instead of leaking a second one; an unrelated operation ID
// resolves to nothing; and a destroyed sandbox leaves the operation with no live
// resource. This is the crash-window recovery contract, and only the live API
// can prove that the label filter and `nextCursor` paging shape are real.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_DAYTONA_TESTS=1 and DAYTONA_API_KEY"]
async fn daytona_provisioning_operation_is_discoverable_and_idempotent() {
    if !live_daytona_tests_enabled() {
        return;
    }
    require_daytona_credentials();

    let provider = live_provider();
    let spec = live_hand_spec(moa_core::types::hands::SandboxTier::Container);
    let operation_id = spec.provisioning_operation_id;

    let handle = provider
        .provision(spec.clone())
        .await
        .expect("failed to provision Daytona sandbox");

    let result = AssertUnwindSafe(async {
        let discovered = provider.provisioned_hands(operation_id).await?;
        assert_eq!(
            discovered,
            vec![handle.clone()],
            "the durable operation must resolve to exactly the sandbox it created"
        );

        // Resolve-before-create must return the live sandbox, so a retry after a
        // crash between provider create and durable handle persistence cannot
        // strand a second sandbox under the same operation.
        let reprovisioned = provider.provision(spec.clone()).await?;
        assert_eq!(
            reprovisioned, handle,
            "re-provisioning one operation must resolve to its existing sandbox"
        );
        assert_eq!(
            provider.provisioned_hands(operation_id).await?,
            vec![handle.clone()],
            "re-provisioning must not create a second sandbox for the operation"
        );

        // An unrelated operation must resolve to nothing. If Daytona ignored or
        // misparsed the label filter, the live sandbox above would appear here,
        // so this is what makes the positive match meaningful.
        let unrelated = provider
            .provisioned_hands(moa_core::types::identifiers::HandProvisioningOperationId::new())
            .await?;
        assert!(
            unrelated.is_empty(),
            "an unrelated provisioning operation resolved to {unrelated:?}"
        );

        Ok::<(), MoaError>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = destroy_and_wait(&provider, &handle).await;

    match result {
        Ok(Ok(())) => {
            cleanup_result.expect("sandbox cleanup should succeed");
            wait_for_no_provisioned_hands(&provider, operation_id, Duration::from_secs(60))
                .await
                .expect("a destroyed sandbox must leave the operation with no live resource");
        }
        Ok(Err(error)) => {
            cleanup_result.expect("sandbox cleanup should succeed after provider failure");
            panic!("live Daytona provisioning operation test failed: {error}");
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
        let secured = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_one, "content": content_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await
            .expect("first router write should provision a hand");
        let hand_id = secured.hand_id.clone();
        let write = secured.safe_output;
        assert_eq!(
            write.to_text(),
            format!("[new file created: {file_one}, 1 lines]")
        );
        hand_id.expect("cloud hand execution should return a hand id")
    };

    let handle_one = HandHandle::daytona(handle_one_id.clone());
    let mut handle_two: Option<HandHandle> = None;
    let test_result = AssertUnwindSafe(async {
        let secured_2 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;
        let same_hand_id = secured_2.hand_id.clone();
        let read = secured_2.safe_output;
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
        let secured_3 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_one,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_read".to_string(),
                    input: json!({ "path": file_one }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;
        let resumed_hand_id = secured_3.hand_id.clone();
        let resumed_read = secured_3.safe_output;
        assert_eq!(resumed_hand_id.as_deref(), Some(handle_one_id.as_str()));
        assert_eq!(resumed_read.to_text(), content_one);

        let secured_4 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "file_write".to_string(),
                    input: json!({ "path": file_two, "content": content_two }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;

        let hand_two_id = secured_4.hand_id.clone();

        let second_write = secured_4.safe_output;
        assert_eq!(
            second_write.to_text(),
            format!("[new file created: {file_two}, 1 lines]")
        );
        let hand_two_id = hand_two_id.expect("second session should receive a distinct hand");
        assert_ne!(hand_two_id, handle_one_id);
        handle_two = Some(HandHandle::daytona(hand_two_id.clone()));

        let secured_5 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": "sh -lc 'printf router-bash'", "timeout_secs": 60 }),
                },
                tool_call_id: ToolCallId::new(),
                active_canary: None,
                catalog: None,
                scope: moa_hands::ToolCallScope::unbounded(),
            })
            .await?;

        let bash = secured_5.safe_output;
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

/// A live-provider hand spec: unrestricted egress with a 5-minute idle window
/// inside a 10-minute hard lifetime, which is what both cloud providers can
/// actually enforce.
fn live_hand_spec(tier: moa_core::types::hands::SandboxTier) -> HandSpec {
    use moa_core::types::hands::{
        BuiltinPolicyRevision, CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit,
        SandboxPolicySnapshot, SandboxProfile, resolve_effective_sandbox_profile,
    };

    let seconds = |value: u64| LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(value).expect("nonzero seconds"),
    };
    let profile = SandboxProfile::new(
        CpuLimit::Unbounded,
        MemoryLimit::Unbounded,
        DiskLimit::Unbounded,
        EgressPolicy::Unrestricted,
        seconds(300),
        seconds(600),
    )
    .expect("live profile should validate");
    let effective_profile = resolve_effective_sandbox_profile(
        &SandboxPolicySnapshot::new("live-deployment", profile).expect("deployment snapshot"),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::TenantUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::AgentUnset),
        &SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        &SandboxPolicySnapshot::origin(CallOrigin::Production),
        "live-capabilities-v1",
    )
    .expect("live policy resolution should succeed");
    HandSpec {
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: tier,
        image: None,
        env: std::collections::HashMap::new(),
        workspace_mount: None,
        effective_profile,
    }
}
