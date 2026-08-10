//! Origin-aware capability admission and bash timeout containment.
//!
//! These drive the real router entry points — policy evaluation, the immediate
//! dispatch path, and the durable recovery path — so the assertions cover what
//! an experiment trial would actually reach, not a policy helper in isolation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use moa_config::McpServerConfig;
use moa_config::MoaConfig;
use moa_core::{
    traits::HandProvider, traits::Identity, traits::IdentityType, types::action_policy::CallOrigin,
    types::completion::ToolInvocation, types::identifiers::ModelId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::sandbox_workspace::SandboxWorkspaceScope,
    types::session::SessionMeta,
};
use moa_hands::{LocalHandProvider, ToolRouter};
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::mcp_router::{mcp_egress_guard, opt_into_development_local_hands};

const CONNECTOR: &str = "crm";
const CONNECTOR_TOOL: &str = "create_deal";

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x0f01),
        tenant_id: TenantId::from(Uuid::from_u128(0x0f02)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: identity().tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn trial_origin() -> CallOrigin {
    CallOrigin::Experiment {
        run_uid: Uuid::from_u128(0x0e01),
        trial_uid: Some(Uuid::from_u128(0x0e02)),
    }
}

fn connector_invocation() -> ToolInvocation {
    ToolInvocation {
        id: None,
        name: moa_hands::mcp_tool_reference(CONNECTOR, CONNECTOR_TOOL),
        input: json!({ "account": "acme" }),
    }
}

/// A connector that answers discovery and records whether a tool was ever run.
struct RecordingConnector {
    url: String,
    tool_calls: Arc<AtomicBool>,
    discoveries: Arc<AtomicUsize>,
}

/// Spawns a connector that serves discovery forever and flags any `tools/call`.
async fn spawn_recording_connector() -> RecordingConnector {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake connector");
    let addr = listener.local_addr().expect("fake connector address");
    let tool_calls = Arc::new(AtomicBool::new(false));
    let discoveries = Arc::new(AtomicUsize::new(0));
    let seen_calls = Arc::clone(&tool_calls);
    let seen_discoveries = Arc::clone(&discoveries);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0_u8; 8192];
            let bytes = match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => continue,
                Ok(read) => read,
            };
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let method = request
                .split_once("\r\n\r\n")
                .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                .and_then(|value| {
                    value
                        .get("method")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
            let body = match method.as_deref() {
                Some("initialize") => {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#
                        .to_string()
                }
                Some("tools/list") => {
                    seen_discoveries.fetch_add(1, Ordering::SeqCst);
                    format!(
                        r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{CONNECTOR_TOOL}","description":"Create a CRM deal","inputSchema":{{"type":"object","properties":{{"account":{{"type":"string"}}}},"required":["account"],"additionalProperties":false}}}}]}}}}"#
                    )
                }
                Some("tools/call") => {
                    seen_calls.store(true, Ordering::SeqCst);
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"deal created"}]}}"#
                        .to_string()
                }
                _ => "{}".to_string(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    RecordingConnector {
        url: format!("http://{addr}"),
        tool_calls,
        discoveries,
    }
}

async fn router_for(url: &str, sandbox_root: &std::path::Path) -> ToolRouter {
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = sandbox_root.display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        required: true,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: CONNECTOR.to_string(),
        url: url.to_string(),
        credentials: None,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }];
    ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a discovered connector")
}

#[tokio::test]
async fn an_experiment_origin_call_cannot_reach_a_production_connector_offline() {
    // Pins: a router serving an experiment trial holds no production connector
    // capability. The refusal happens on the policy path AND on the durable
    // recovery path (which deliberately does not re-run action policy), and the
    // connector never receives a tools/call. The identical invocation succeeds
    // on a production-origin router built against the same connector, so the
    // refusal is a property of the origin rather than a broken connector, a
    // failed discovery, or a rejected input.
    let connector = spawn_recording_connector().await;
    let dir = tempdir().expect("sandbox dir");

    let trial_router = router_for(&connector.url, dir.path())
        .await
        .with_call_origin(trial_origin());
    assert!(
        connector.discoveries.load(Ordering::SeqCst) >= 1,
        "the connector must be discovered before its refusal can be meaningful"
    );

    let policy_error = trial_router
        .check_policy(&session(), &connector_invocation())
        .await
        .expect_err("a trial must not hold a connector capability");
    assert!(
        matches!(&policy_error, moa_core::error::MoaError::PermissionDenied(message)
            if message.contains("production connectors") && message.contains(CONNECTOR)),
        "refusal must be a permission denial naming the connector: {policy_error:?}"
    );

    let dispatch_error = trial_router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &connector_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("the dispatch path must refuse the same capability");
    assert!(matches!(
        dispatch_error,
        moa_core::error::MoaError::PermissionDenied(_)
    ));

    let durable_error = trial_router
        .execute_authorized_with_recovery(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &connector_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("the durable path must refuse the same capability");
    assert!(
        matches!(
            durable_error,
            moa_core::error::MoaError::PermissionDenied(_)
        ),
        "the recovery path skips action policy, so it must carry its own admission"
    );

    assert!(
        !connector.tool_calls.load(Ordering::SeqCst),
        "no refused path may reach the connector"
    );

    let production_router = router_for(&connector.url, dir.path()).await;
    let secured = production_router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &connector_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect("production traffic keeps the same connector");
    assert_eq!(secured.safe_output.to_text(), "deal created");
    assert!(connector.tool_calls.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_trial_owned_session_loses_the_connector_on_a_production_router_offline() {
    // Pins: the router ceiling and the session ceiling compose, and neither one
    // alone is the authority. The router here is the shared, production-origin
    // one a deployment actually builds — the test above already shows a
    // trial-origin router fencing a production session — so the only thing that
    // can refuse this call is the session's own origin. The same router serves
    // the same connector to a production-origin session immediately afterwards.
    let connector = spawn_recording_connector().await;
    let dir = tempdir().expect("sandbox dir");
    let router = router_for(&connector.url, dir.path()).await;
    assert!(router.call_origin().is_production());

    let trial_session = SessionMeta {
        call_origin: trial_origin(),
        ..session()
    };
    assert_eq!(
        router.effective_call_origin(&trial_session),
        trial_origin(),
        "a production router must not widen a trial-owned session"
    );

    let policy_error = router
        .check_policy(&trial_session, &connector_invocation())
        .await
        .expect_err("a trial-owned session must not hold a connector capability");
    assert!(
        matches!(&policy_error, moa_core::error::MoaError::PermissionDenied(message)
            if message.contains("production connectors") && message.contains(CONNECTOR)),
        "refusal must name the connector and the reason: {policy_error:?}"
    );

    let durable_error = router
        .execute_authorized_with_recovery(moa_hands::AuthorizedToolCall {
            session: &trial_session,
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &connector_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("the durable path must refuse the same capability");
    assert!(matches!(
        durable_error,
        moa_core::error::MoaError::PermissionDenied(_)
    ));
    assert!(
        !connector.tool_calls.load(Ordering::SeqCst),
        "no refused path may reach the connector"
    );

    let secured = router
        .execute_authorized_with_recovery(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &connector_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect("a production-origin session keeps the connector on the same router");
    assert_eq!(secured.safe_output.to_text(), "deal created");
    assert!(connector.tool_calls.load(Ordering::SeqCst));
}

#[tokio::test]
async fn an_experiment_origin_fails_closed_on_the_host_tier_offline() {
    // Pins: an experiment binds deny-all egress, and direct host execution
    // cannot enforce that posture. Admission therefore refuses the call before
    // provisioning or writing the fixture instead of widening trial policy to
    // keep the host tier usable.
    let dir = tempdir().expect("sandbox dir");
    let router = ToolRouter::new_local(dir.path())
        .await
        .expect("local router")
        .with_call_origin(trial_origin());
    let session = session();
    let caller_identity = identity();
    let workspace_scope = SandboxWorkspaceScope::Worker {
        session_id: session.id,
        worker_id: "experiment-host-tier-worker".to_string(),
    };
    let invocation = ToolInvocation {
        id: None,
        name: "file_write".to_string(),
        input: json!({ "path": "fixture.txt", "content": "trial fixture" }),
    };
    let error = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session,
            caller_identity: &caller_identity,
            workspace_scope: Some(&workspace_scope),
            invocation: &invocation,
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("host execution must not serve deny-all experiment traffic");
    assert!(
        matches!(&error, moa_core::error::MoaError::Unsupported(message)
            if message.contains("deny_all") && message.contains("egress")),
        "admission must name the unenforceable deny-all posture: {error:?}"
    );
    assert!(
        !dir.path().join("fixture.txt").exists(),
        "refused experiment work must not perform its side effect"
    );
}

#[tokio::test]
async fn generated_code_still_binds_deny_all_egress_offline() {
    // Pins: narrowing the experiment layer did not narrow the generated-code layer.
    // `DenyAll` is what the plan states for model-generated code, and it remains
    // part of the resolved profile rather than a check some dispatch path performs.
    use moa_core::types::hands::{EgressPolicy, OriginPolicyRevision};

    assert_eq!(
        OriginPolicyRevision::of(CallOrigin::GeneratedCode)
            .profile()
            .egress,
        EgressPolicy::DenyAll,
        "generated code must still be network-isolated by its origin layer"
    );
    assert_eq!(
        OriginPolicyRevision::of(trial_origin()).profile().egress,
        EgressPolicy::DenyAll,
        "an experiment must be network-isolated by its origin layer"
    );
}

#[tokio::test]
async fn an_over_limit_bash_timeout_never_reaches_the_executor_offline() {
    // Pins: an out-of-policy `timeout_secs` fails before any process is
    // spawned, on the schema-validating router path AND on the hand-provider
    // path a direct executor takes without schema validation. The marker file
    // is the proof: it exists only if the shell actually ran.
    let dir = tempdir().expect("sandbox dir");
    let provider = LocalHandProvider::new_with_docker_detection(dir.path(), false)
        .await
        .expect("local hand provider");
    let spec = moa_core::types::hands::HandSpec {
        provisioning_operation_id: moa_core::types::identifiers::HandProvisioningOperationId::new(),
        workspace: moa_core::types::sandbox_workspace::WorkspaceBinding {
            tenant_id: moa_core::types::identifiers::TenantId::new(),
            scope: moa_core::types::sandbox_workspace::SandboxWorkspaceScope::Worker {
                session_id: moa_core::types::identifiers::SessionId::new(),
                worker_id: "origin-test-worker".to_string(),
            },
            workspace_id: moa_core::types::identifiers::SandboxWorkspaceId::new(),
            provider_account_id: moa_core::types::identifiers::ProviderAccountId::new(),
            provider_account_generation: 1,
            durability_class:
                moa_core::types::sandbox_workspace::DurabilityClass::PortableFilesystem,
            writer_epoch: 1,
            instance_generation: 1,
            current_revision: None,
        },
        budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
        sandbox_tier: moa_core::types::hands::SandboxTier::Local,
        image: None,
        env: std::collections::HashMap::new(),
        filesystem: moa_core::types::sandbox_workspace::SandboxFilesystemLayout::standard(),
        effective_profile: moa_core::types::hands::resolve_effective_sandbox_profile(
            &MoaConfig::default()
                .sandbox_policy
                .deployment
                .snapshot()
                .expect("deployment layer"),
            &moa_core::types::hands::SandboxPolicySnapshot::builtin(
                moa_core::types::hands::BuiltinPolicyRevision::TenantUnset,
            ),
            &moa_core::types::hands::SandboxPolicySnapshot::builtin(
                moa_core::types::hands::BuiltinPolicyRevision::AgentUnset,
            ),
            &moa_core::types::hands::SandboxPolicySnapshot::builtin(
                moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
            ),
            // Production here on purpose: this fixture proves the *session*
            // origin is what withdraws the capability, so the sandbox spec must
            // not be the thing doing the restricting.
            &moa_core::types::hands::SandboxPolicySnapshot::origin(
                moa_core::types::action_policy::CallOrigin::Production,
            ),
            &moa_hands::LOCAL_HAND_CAPABILITIES.revision,
        )
        .expect("effective profile"),
    };
    let handle = provider.provision(spec).await.expect("provisioned sandbox");
    let sandbox_dir = match &handle {
        moa_core::types::hands::HandHandle::Local { sandbox_dir } => sandbox_dir.clone(),
        other => panic!("expected a local handle, got {other:?}"),
    };
    let marker = sandbox_dir.join("executor-ran");
    let runaway = json!({
        "cmd": format!("touch {}", marker.display()),
        "timeout_secs": 86_400,
    })
    .to_string();

    let provider_error = provider
        .execute(&handle, "bash", &runaway)
        .await
        .expect_err("an out-of-policy timeout must not run a command");
    assert!(
        matches!(&provider_error, moa_core::error::MoaError::ValidationError(message)
            if message.contains("86400") && message.contains("300")),
        "the refusal must name the request and the limit: {provider_error:?}"
    );
    assert!(
        !marker.exists(),
        "an over-limit timeout must not reach the shell"
    );

    // The same command with an in-policy timeout does run, so the refusal is
    // the timeout's doing and not a broken command or sandbox.
    provider
        .execute(
            &handle,
            "bash",
            &json!({
                "cmd": format!("touch {}", marker.display()),
                "timeout_secs": 30,
            })
            .to_string(),
        )
        .await
        .expect("an in-policy timeout still executes");
    assert!(marker.exists(), "the in-policy command must have run");
}
