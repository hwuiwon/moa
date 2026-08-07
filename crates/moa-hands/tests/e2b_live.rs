// No offline counterpart possible because: this live file verifies real E2B sandbox provisioning, lifecycle, and filesystem isolation semantics that a local HTTP mock cannot emulate.

//! Live E2B integration tests.
//!
//! These tests are ignored by default because they provision real E2B sandboxes
//! and require valid credentials in the environment.

use std::time::Duration;
use std::{panic::AssertUnwindSafe, panic::resume_unwind};

use futures_util::FutureExt;
use moa_config::{CloudHandsConfig, MoaConfig, SandboxProfileConfig};
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
use moa_hands::{E2BHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
use tokio::time::{Instant, sleep};
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
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c341),
        tenant_id: TenantId::from(Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c342)),
        api_key_id: None,
        acting_on_behalf_of: None,
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

fn live_e2b_tests_enabled() -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    std::env::var("MOA_RUN_LIVE_E2B_TESTS")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_e2b_credentials() {
    assert!(
        std::env::var("E2B_API_KEY").is_ok_and(|value| !value.trim().is_empty()),
        "MOA_RUN_LIVE_E2B_TESTS=1 requires E2B_API_KEY"
    );
}

fn live_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        e2b_api_key: Some(std::env::var("E2B_API_KEY").expect("E2B_API_KEY")),
        e2b_api_url: Some(
            std::env::var("E2B_API_URL").unwrap_or_else(|_| "https://api.e2b.dev".to_string()),
        ),
        e2b_domain: Some(std::env::var("E2B_DOMAIN").unwrap_or_else(|_| "e2b.app".to_string())),
        e2b_template: Some(std::env::var("E2B_TEMPLATE").unwrap_or_else(|_| "base".to_string())),
        ..CloudHandsConfig::default()
    });
    config.sandbox_policy.deployment = live_sandbox_profile_config();
    config
}

fn live_sandbox_profile_config() -> SandboxProfileConfig {
    use moa_core::types::hands::{CpuLimit, DiskLimit, EgressPolicy, LifetimeLimit, MemoryLimit};

    let seconds = |value: u64| LifetimeLimit::Bounded {
        seconds: std::num::NonZeroU64::new(value).expect("nonzero seconds"),
    };
    SandboxProfileConfig {
        revision: "e2b-live-sandbox-v1".to_string(),
        cpu: CpuLimit::Unbounded,
        memory: MemoryLimit::Unbounded,
        ephemeral_disk: DiskLimit::Unbounded,
        egress: EgressPolicy::Unrestricted,
        idle_timeout: seconds(300),
        max_lifetime: seconds(600),
    }
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

/// Waits until a durable provisioning operation resolves to no live sandbox.
///
/// The list API is only bounded-consistent after a destroy, so a destroyed
/// sandbox is allowed to linger in the metadata-filtered listing briefly.
async fn wait_for_no_provisioned_hands(
    provider: &E2BHandProvider,
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
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_provider_round_trip() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let provider = live_provider();

    let unsupported = provider
        .provision(live_hand_spec(
            moa_core::types::hands::SandboxTier::Container,
        ))
        .await;
    assert!(matches!(unsupported, Err(MoaError::Unsupported(_))));

    let handle = provider
        .provision(live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM))
        .await
        .expect("failed to provision E2B sandbox");

    let file_path = format!("tmp/moa-e2b-live-{}.txt", Uuid::now_v7().simple());
    let marker = format!("marker-{}", Uuid::now_v7().simple());

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

// Pins: a live sandbox created under a durable provisioning operation ID is
// discoverable by that ID through E2B's real metadata-filtered list API;
// re-provisioning the same operation resolves to the same sandbox instead of
// leaking a second one; an unrelated operation ID resolves to nothing; and a
// destroyed sandbox leaves the operation with no live resource. This is the
// crash-window recovery contract, and only the live API can prove that the
// metadata filter is real rather than silently ignored.
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_provisioning_operation_is_discoverable_and_idempotent() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let provider = live_provider();
    let spec = live_hand_spec(moa_core::types::hands::SandboxTier::MicroVM);
    let operation_id = spec.provisioning_operation_id;

    let handle = provider
        .provision(spec.clone())
        .await
        .expect("failed to provision E2B sandbox");

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

        // An unrelated operation must resolve to nothing. If E2B ignored or
        // misparsed the metadata filter, the live sandbox above would appear
        // here, so this is what makes the positive match meaningful.
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
            panic!("live E2B provisioning operation test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("sandbox cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_E2B_TESTS=1 and E2B_API_KEY"]
async fn e2b_router_reuses_and_isolates() {
    if !live_e2b_tests_enabled() {
        return;
    }
    require_e2b_credentials();

    let mut config = live_config();
    let temp = tempdir().expect("tempdir");
    config.local.sandbox_dir = temp.path().join("sandbox").display().to_string();

    // This fixture manually destroys every live sandbox below, so it owns the
    // cleanup obligation that the production composition root assigns to the
    // durable reaper. Declare that owner before bounded-idle admission.
    let router = ToolRouter::from_config(&config, None, None)
        .await
        .expect("router should load E2B from config")
        .with_hand_lease_reaper();
    let provider = E2BHandProvider::from_config(&config).expect("provider from config");

    let session_one = session("one");
    let session_two = session("two");
    let file_one = format!("tmp/moa-e2b-router-one-{}.txt", Uuid::now_v7().simple());
    let file_two = format!("tmp/moa-e2b-router-two-{}.txt", Uuid::now_v7().simple());
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

    let handle_one = HandHandle::e2b(handle_one_id.clone());
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
        assert!(read.to_text().contains(&content_one));

        provider.pause(&handle_one).await?;
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
        assert!(resumed_read.to_text().contains(&content_one));

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
        handle_two = Some(HandHandle::e2b(hand_two_id.clone()));

        let missing_read = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
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
            .await;
        match missing_read {
            Ok(secured) => {
                let output = secured.safe_output;
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

        let secured_5 = router
            .execute_authorized(moa_hands::AuthorizedToolCall {
                session: &session_two,
                caller_identity: &identity(),
                worker_id: None,
                invocation: &ToolInvocation {
                    id: None,
                    name: "bash".to_string(),
                    input: json!({ "cmd": "printf router-bash", "timeout_secs": 60 }),
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
            panic!("live E2B router test failed: {error}");
        }
        Err(panic) => {
            cleanup_result.expect("router cleanup should succeed after panic");
            resume_unwind(panic);
        }
    }
}

/// A live-provider hand spec with unrestricted egress, a 5-minute idle window,
/// and a 10-minute provider-enforced hard lifetime.
fn live_hand_spec(tier: moa_core::types::hands::SandboxTier) -> HandSpec {
    use moa_core::types::action_policy::CallOrigin;
    use moa_core::types::hands::{
        BuiltinPolicyRevision, SandboxPolicySnapshot, resolve_effective_sandbox_profile,
    };

    let effective_profile = resolve_effective_sandbox_profile(
        &live_sandbox_profile_config()
            .snapshot()
            .expect("live deployment snapshot"),
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
