use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use moa_config::CloudHandsConfig;
use moa_config::McpServerCredentialScope;
use moa_config::MoaConfig;
use moa_config::SecurityProfile;
use moa_core::{
    error::MoaError, error::Result, traits::BuiltInTool, traits::Identity, traits::IdentityType,
    traits::ToolContext, types::action_policy::ActionClass,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionPolicyRule,
    types::action_policy::ActionRuleScope, types::action_policy::RiskLevel,
    types::completion::ToolInvocation, types::identifiers::ModelId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::identifiers::UserId, types::session::SessionMeta,
    types::tools::IdempotencyClass, types::tools::ToolDiffStrategy, types::tools::ToolInputShape,
    types::tools::ToolOutput, types::tools::ToolPolicySpec,
};
use moa_hands::{McpDiscoveredTool, ToolRegistry, ToolRouter};
use moa_security::ActionPolicyRuleStore;
use opentelemetry::Value;
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor, SpanData,
};
use serde_json::json;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

const INPUT_SECRET: &str = "sk-test-input-secret-9a7b";
const ERROR_SECRET: &str = "stderr-secret-71d2";
const TOOL_ERROR_OUTPUT_STATUS: &str = "tool returned error output";
const TOOL_EXECUTION_FAILED_STATUS: &str = "tool execution failed";

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: identity().tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c421),
        tenant_id: TenantId::from(uuid::Uuid::from_u128(
            0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c422,
        )),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn local_config() -> MoaConfig {
    let mut config = MoaConfig::default();
    config.local.docker_enabled = true;
    config.security_profile = SecurityProfile::Local;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("local".to_string()),
        ..CloudHandsConfig::default()
    });
    config
}

/// Returns a fully valid `cloud` profile config: deny default, e2b backend, and
/// a present backend credential. Individual tests break exactly one requirement.
fn cloud_config() -> MoaConfig {
    let mut config = MoaConfig {
        security_profile: SecurityProfile::Cloud,
        ..MoaConfig::default()
    };
    config.permissions.default_effect = ActionPolicyEffect::Deny;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("e2b".to_string()),
        e2b_api_key: Some("MOA_TEST_E2B_KEY".to_string()),
        ..CloudHandsConfig::default()
    });
    config
}

fn cloud_rule_store() -> Arc<dyn ActionPolicyRuleStore> {
    Arc::new(StaticRuleStore { rules: Vec::new() })
}

fn config_error_message(error: MoaError) -> String {
    match error {
        MoaError::ConfigError(message) => message,
        other => panic!("expected a ConfigError, got {other:?}"),
    }
}

struct SecretErrorTool;

#[async_trait]
impl BuiltInTool for SecretErrorTool {
    fn name(&self) -> &'static str {
        "secret_error"
    }

    fn description(&self) -> &'static str {
        "test-only tool that returns a secret-bearing error"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }

    fn policy_spec(&self) -> ToolPolicySpec {
        ToolPolicySpec {
            risk_level: RiskLevel::Low,
            default_effect: ActionPolicyEffect::Allow,
            action_class: ActionClass::Read,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        }
    }

    fn idempotency_class(&self) -> IdempotencyClass {
        IdempotencyClass::Idempotent
    }

    async fn execute(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolContext<'_>,
    ) -> Result<ToolOutput> {
        Err(MoaError::ToolError(format!(
            "failed with secret body {ERROR_SECRET}"
        )))
    }
}

async fn capture_spans<F, Fut>(emit: F) -> Vec<SpanData>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("moa-hands-security-defaults");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(layer);

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        emit().await;
    }

    let _ = provider.force_flush();
    exporter
        .get_finished_spans()
        .expect("in-memory exporter should return finished spans")
}

fn find_span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let matches = spans
        .iter()
        .filter(|span| span.name.as_ref() == name)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one captured span named `{name}`, found {} (names: {:?})",
        matches.len(),
        spans
            .iter()
            .map(|span| span.name.as_ref())
            .collect::<Vec<_>>(),
    );
    matches[0]
}

fn attr_string(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

fn attr_i64(span: &SpanData, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            Value::I64(value) => Some(*value),
            _ => None,
        })
}

fn attr_bool(span: &SpanData, key: &str) -> Option<bool> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            Value::Bool(value) => Some(*value),
            _ => None,
        })
}

fn status_description(span: &SpanData) -> Option<&str> {
    match &span.status {
        Status::Error { description } => Some(description.as_ref()),
        Status::Unset | Status::Ok => None,
    }
}

fn assert_no_attr(span: &SpanData, key: &str) {
    assert!(
        span.attributes.iter().all(|kv| kv.key.as_str() != key),
        "span `{}` should not include `{key}`",
        span.name
    );
}

fn assert_spans_do_not_contain(spans: &[SpanData], secrets: &[&str]) {
    for span in spans {
        let mut emitted = span.name.to_string();
        for attr in &span.attributes {
            emitted.push_str(attr.key.as_str());
            emitted.push_str(attr.value.as_str().as_ref());
        }
        if let Some(description) = status_description(span) {
            emitted.push_str(description);
        }
        for secret in secrets {
            assert!(
                !emitted.contains(secret),
                "captured span `{}` leaked secret `{secret}`",
                span.name
            );
        }
    }
}

fn assert_trace_body_env_disabled() {
    assert!(
        std::env::var_os("MOA_TRACE_TOOL_OUTPUT").is_none(),
        "security default tests require MOA_TRACE_TOOL_OUTPUT to be unset"
    );
}

#[tokio::test]
async fn cloud_profile_rejects_the_local_hand_provider() {
    // Pins: the cloud profile is the fail-closed posture, so a local host route
    // is refused before the router is returned even when every other cloud
    // requirement is satisfied.
    let mut config = cloud_config();
    config
        .cloud
        .hands
        .get_or_insert_with(CloudHandsConfig::default)
        .default_provider = Some("local".to_string());
    let dir = tempdir().expect("tempdir should be created");
    config.local.sandbox_dir = dir.path().display().to_string();

    let error = match ToolRouter::from_config(&config, None, Some(cloud_rule_store()), None).await {
        Ok(_) => panic!("cloud profile must reject a local hand route"),
        Err(error) => error,
    };

    let message = config_error_message(error);
    assert!(
        message.contains("security_profile=cloud"),
        "error must name the profile, got: {message}"
    );
    assert!(
        message.contains("local hand provider"),
        "error must name the rejected provider, got: {message}"
    );
}

#[tokio::test]
async fn cloud_profile_rejects_an_allow_permission_default() {
    // Pins: a cloud deployment cannot serve with a permissive permission
    // posture; the deny default is a construction-time requirement, not a
    // runtime suggestion.
    let mut config = cloud_config();
    config.permissions.default_effect = ActionPolicyEffect::Allow;

    let error = match ToolRouter::from_config(&config, None, Some(cloud_rule_store()), None).await {
        Ok(_) => panic!("cloud profile must reject an allow permission default"),
        Err(error) => error,
    };

    let message = config_error_message(error);
    assert!(
        message.contains("permissions.default_effect=deny"),
        "error must name the required posture, got: {message}"
    );
}

#[tokio::test]
async fn cloud_profile_rejects_a_missing_rule_store_owner() {
    // Pins: a deny-by-default cloud deployment with no persisted-rule owner
    // could never authorize any action, so construction fails instead of
    // serving a router that denies everything.
    let error = match ToolRouter::from_config(&cloud_config(), None, None, None).await {
        Ok(_) => panic!("cloud profile must reject a missing rule store owner"),
        Err(error) => error,
    };

    let message = config_error_message(error);
    assert!(
        message.contains("action-policy rule store"),
        "error must name the missing owner, got: {message}"
    );
}

#[tokio::test]
async fn cloud_profile_rejects_a_selected_backend_without_credentials() {
    // Pins: selecting a cloud sandbox without its credential fails closed at
    // construction rather than at the first tool call.
    let mut config = cloud_config();
    config
        .cloud
        .hands
        .get_or_insert_with(CloudHandsConfig::default)
        .e2b_api_key = None;

    let error = match ToolRouter::from_config(&config, None, Some(cloud_rule_store()), None).await {
        Ok(_) => panic!("cloud profile must reject a backend without credentials"),
        Err(error) => error,
    };

    let message = config_error_message(error);
    assert!(
        message.contains("requires credentials") && message.contains("e2b"),
        "error must name the uncredentialed backend, got: {message}"
    );
}

#[tokio::test]
async fn cloud_profile_constructs_with_deny_default_owner_and_credentialed_backend() {
    // Pins: the four cloud requirements together are sufficient — a valid cloud
    // deployment builds a router whose hand tools target the cloud backend and
    // never register the local host provider.
    let router = ToolRouter::from_config(&cloud_config(), None, Some(cloud_rule_store()), None)
        .await
        .expect("a fully configured cloud profile should construct");

    assert!(router.has_tool("bash"));
    assert!(
        !router.has_tool("__local_host__"),
        "cloud router must not expose a local host tool"
    );
}

#[tokio::test]
async fn local_route_with_opt_in_registers_local_hands() {
    // Pins: the local profile keeps the host hand provider usable for offline
    // development with no rule-store owner required.
    let mut config = local_config();
    config.local.docker_enabled = false;
    let dir = tempdir().expect("tempdir should be created");
    config.local.sandbox_dir = dir.path().display().to_string();

    let router = ToolRouter::from_config(&config, None, None, None)
        .await
        .expect("local opt-in should allow router construction");
    assert!(router.has_tool("file_write"));
    assert!(router.has_tool("file_read"));
    let session = session();

    router
        .execute_authorized(
            &session,
            &identity(),
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "allowed local opt-in" }),
            },
            ToolCallId::new(),
            None,
        )
        .await
        .expect("opted-in local file_write should execute");
    let secured = router
        .execute_authorized(
            &session,
            &identity(),
            &ToolInvocation {
                id: None,
                name: "file_read".to_string(),
                input: json!({ "path": "notes.txt" }),
            },
            ToolCallId::new(),
            None,
        )
        .await
        .expect("opted-in local file_read should execute");
    let output = secured.safe_output;

    assert_eq!(output.to_text(), "allowed local opt-in");
}

#[test]
fn bash_default_policy_is_not_allow() {
    // Pins: high-risk command execution cannot bypass review through descriptor defaults.
    let registry = ToolRegistry::default_local();
    let bash = registry
        .get("bash")
        .expect("default registry should include bash");

    assert_ne!(bash.policy.default_effect, ActionPolicyEffect::Allow);
    assert_eq!(bash.policy.default_effect, ActionPolicyEffect::AdminReview);
}

#[tokio::test(flavor = "current_thread")]
async fn tool_telemetry_redacts_raw_input_and_execution_errors_by_default() {
    // Pins: tool execution spans keep correlation metadata but do not attach raw
    // input or raw error strings unless body tracing is explicitly enabled.
    assert_trace_body_env_disabled();
    let input = json!({ "api_key": INPUT_SECRET, "operation": "fail" });
    let serialized_input =
        serde_json::to_string(&input).expect("test input should serialize to JSON");

    let spans = capture_spans(|| {
        let input = input.clone();
        async move {
            let mut registry = ToolRegistry::new();
            registry.register_builtin(Arc::new(SecretErrorTool));
            let router = ToolRouter::new(registry, HashMap::new());
            let error = router
                .execute_authorized(
                    &session(),
                    &identity(),
                    &ToolInvocation {
                        id: None,
                        name: "secret_error".to_string(),
                        input,
                    },
                    ToolCallId::new(),
                    None,
                )
                .await
                .expect_err("test tool should fail with a secret-bearing error");

            assert!(
                matches!(error, MoaError::ToolError(message) if message.contains(ERROR_SECRET)),
                "setup should prove the raw error contained the fake secret"
            );
        }
    })
    .await;

    let span = find_span(&spans, "execute_tool secret_error");
    assert_eq!(
        attr_string(span, "gen_ai.tool.name").as_deref(),
        Some("secret_error")
    );
    assert_eq!(
        attr_i64(span, "moa.tool.input.bytes"),
        Some(serialized_input.len() as i64)
    );
    assert_eq!(
        attr_string(span, "moa.tool.input.hash")
            .expect("input hash should be recorded")
            .len(),
        16
    );
    assert_eq!(attr_bool(span, "moa.tool.success"), Some(false));
    assert!(
        attr_i64(span, "moa.tool.duration_ms").is_some(),
        "tool duration should be recorded"
    );
    assert_eq!(status_description(span), Some(TOOL_EXECUTION_FAILED_STATUS));
    assert_no_attr(span, "moa.tool.input");
    assert_spans_do_not_contain(&spans, &[INPUT_SECRET, ERROR_SECRET]);
}

#[tokio::test(flavor = "current_thread")]
async fn local_hand_error_output_spans_redact_bodies_by_default() {
    // Pins: error ToolOutput bodies remain available to the caller, while the
    // local provider span records fixed status text without raw tool output.
    assert_trace_body_env_disabled();
    let command = format!("printf '%s\\n' '{ERROR_SECRET}' >&2; exit 7");
    let input = json!({ "cmd": command });

    let spans = capture_spans(|| {
        let input = input.clone();
        async move {
            let mut config = local_config();
            config.local.docker_enabled = false;
            let dir = tempdir().expect("tempdir should be created");
            config.local.sandbox_dir = dir.path().display().to_string();
            let router = ToolRouter::from_config(&config, None, None, None)
                .await
                .expect("local opt-in should allow router construction");
            let secured_2 = router
                .execute_authorized(
                    &session(),
                    &identity(),
                    &ToolInvocation {
                        id: None,
                        name: "bash".to_string(),
                        input,
                    },
                    ToolCallId::new(),
                    None,
                )
                .await
                .expect("failing bash command should still return a ToolOutput");
            let output = secured_2.safe_output;

            assert!(
                output.is_error,
                "setup command should produce an error output"
            );
            assert!(
                output.to_text().contains(ERROR_SECRET),
                "setup should prove the raw output contained the fake secret"
            );
        }
    })
    .await;

    let hand_span = find_span(&spans, "hand.execute local/bash");
    assert_eq!(
        attr_string(hand_span, "moa.hand.provider").as_deref(),
        Some("local")
    );
    assert_eq!(
        attr_string(hand_span, "moa.hand.tier").as_deref(),
        Some("local")
    );
    assert_eq!(
        status_description(hand_span),
        Some(TOOL_ERROR_OUTPUT_STATUS)
    );
    assert_spans_do_not_contain(&spans, &[ERROR_SECRET]);
}

/// In-memory action-policy rule store returning fixed rules for a tool.
struct StaticRuleStore {
    rules: Vec<ActionPolicyRule>,
}

#[async_trait]
impl ActionPolicyRuleStore for StaticRuleStore {
    async fn list_action_policy_rules_for_tool(
        &self,
        _tenant_id: &TenantId,
        _user_id: &UserId,
        tool: &str,
    ) -> Result<Vec<ActionPolicyRule>> {
        Ok(self
            .rules
            .iter()
            .filter(|rule| rule.tool == tool)
            .cloned()
            .collect())
    }

    async fn upsert_action_policy_rule(&self, _rule: ActionPolicyRule) -> Result<()> {
        Ok(())
    }

    async fn delete_action_policy_rule(
        &self,
        _tenant_id: &TenantId,
        _user_id: Option<&UserId>,
        _tool: &str,
        _pattern: &str,
    ) -> Result<()> {
        Ok(())
    }
}

fn discovered_mcp_tool(name: &str) -> McpDiscoveredTool {
    McpDiscoveredTool {
        name: name.to_string(),
        description: "third-party MCP tool".to_string(),
        input_schema: json!({ "type": "object" }),
    }
}

fn mcp_invocation(name: &str) -> ToolInvocation {
    ToolInvocation {
        id: None,
        name: name.to_string(),
        input: json!({}),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ungranted_mcp_tool_is_denied_under_the_cloud_deny_default() {
    // Pins: an MCP/third-party tool declares no intrinsic gate of its own, so the
    // deployment posture gates it. Under the cloud deny-by-default posture an
    // ungranted external tool is denied outright — unvetted external code must
    // not execute unattended — and the deployment default owns the decision.
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool(
            "external-server",
            McpServerCredentialScope::DeploymentOwned,
            discovered_mcp_tool("external_action"),
        )
        .expect("register mcp tool");
    let mut config = MoaConfig::default();
    config.permissions.default_effect = ActionPolicyEffect::Deny;
    let router = ToolRouter::new(registry, HashMap::new()).with_policies(
        moa_security::ActionPolicies::from_config(&config).expect("deny-default policies"),
    );

    let check = router
        .check_policy(&session(), &mcp_invocation("external_action"))
        .await
        .expect("policy check for mcp tool");

    assert_eq!(check.effect, ActionPolicyEffect::Deny);
    assert_eq!(
        check.source,
        moa_core::types::tools::ActionPolicyDecisionSource::DeploymentDefault
    );
}

#[tokio::test(flavor = "current_thread")]
async fn configured_admin_review_still_gates_mcp_tools_that_a_rule_cannot_lift() {
    // Pins: an operator who wants every external tool review-gated configures it
    // through `permissions.admin_review`, which is a floor a tenant Allow rule
    // cannot weaken. This is the supported replacement for the per-tool MCP
    // review fallback that a rule used to be able to downgrade.
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool(
            "external-server",
            McpServerCredentialScope::DeploymentOwned,
            discovered_mcp_tool("external_action"),
        )
        .expect("register mcp tool");
    let session = session();
    let mut config = MoaConfig::default();
    config.permissions.default_effect = ActionPolicyEffect::Deny;
    config.permissions.admin_review = vec!["external_*".to_string()];
    let router = ToolRouter::new(registry, HashMap::new())
        .with_policies(
            moa_security::ActionPolicies::from_config(&config).expect("review-config policies"),
        )
        .with_rule_store(Arc::new(StaticRuleStore {
            rules: vec![ActionPolicyRule {
                id: uuid::Uuid::now_v7(),
                scope: ActionRuleScope::Tenant {
                    tenant_id: session.tenant_id,
                },
                tool: "external_action".to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("tenant grant".to_string()),
                created_by: UserId::new("admin"),
                created_at: chrono::Utc::now(),
            }],
        }));

    let check = router
        .check_policy(&session, &mcp_invocation("external_action"))
        .await
        .expect("policy check for configured-review mcp tool");

    assert_eq!(check.effect, ActionPolicyEffect::AdminReview);
    assert_eq!(
        check.source,
        moa_core::types::tools::ActionPolicyDecisionSource::ConfiguredReview
    );
}

#[tokio::test(flavor = "current_thread")]
async fn builtin_tool_keeps_its_descriptor_default_effect() {
    // Pins: the MCP admin-review default does not change builtin tools, which keep their
    // own considered descriptor default (SecretErrorTool declares Allow).
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(SecretErrorTool));
    let router = ToolRouter::new(registry, HashMap::new());

    let check = router
        .check_policy(&session(), &mcp_invocation("secret_error"))
        .await
        .expect("policy check for builtin tool");

    assert_eq!(check.effect, ActionPolicyEffect::Allow);
}

#[tokio::test(flavor = "current_thread")]
async fn tenant_granted_mcp_tool_resolves_to_allow_under_the_cloud_deny_default() {
    // Pins: the deny-by-default cloud posture is not a ceiling on an explicit
    // scoped grant, so a tenant that deliberately allows one external tool can
    // still run it while every ungranted external tool stays denied.
    let session = session();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool(
            "external-server",
            McpServerCredentialScope::DeploymentOwned,
            discovered_mcp_tool("external_action"),
        )
        .expect("register mcp tool");
    registry
        .register_mcp_tool(
            "external-server",
            McpServerCredentialScope::DeploymentOwned,
            discovered_mcp_tool("external_other"),
        )
        .expect("register second mcp tool");
    let allow_rule = ActionPolicyRule {
        id: uuid::Uuid::now_v7(),
        scope: ActionRuleScope::Tenant {
            tenant_id: session.tenant_id,
        },
        tool: "external_action".to_string(),
        pattern: "*".to_string(),
        effect: ActionPolicyEffect::Allow,
        reason: Some("operator trusts this MCP tool".to_string()),
        created_by: UserId::new("admin"),
        created_at: chrono::Utc::now(),
    };
    let mut config = MoaConfig::default();
    config.permissions.default_effect = ActionPolicyEffect::Deny;
    let router = ToolRouter::new(registry, HashMap::new())
        .with_policies(
            moa_security::ActionPolicies::from_config(&config).expect("deny-default policies"),
        )
        .with_rule_store(Arc::new(StaticRuleStore {
            rules: vec![allow_rule],
        }));

    let granted = router
        .check_policy(&session, &mcp_invocation("external_action"))
        .await
        .expect("policy check for allowed mcp tool");
    assert_eq!(granted.effect, ActionPolicyEffect::Allow);
    assert_eq!(
        granted.source,
        moa_core::types::tools::ActionPolicyDecisionSource::PersistedRule
    );

    let ungranted = router
        .check_policy(&session, &mcp_invocation("external_other"))
        .await
        .expect("policy check for ungranted mcp tool");
    assert_eq!(ungranted.effect, ActionPolicyEffect::Deny);
}

#[tokio::test(flavor = "current_thread")]
async fn a_rule_never_makes_an_unregistered_tool_visible() {
    // Pins: policy evaluation happens only after the tool resolves in the
    // registry, so a persisted Allow rule for a filtered or never-registered
    // tool cannot conjure it into existence.
    let session = session();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool(
            "external-server",
            McpServerCredentialScope::DeploymentOwned,
            discovered_mcp_tool("external_action"),
        )
        .expect("register mcp tool");
    registry.retain_only(["external_action"]);
    let router =
        ToolRouter::new(registry, HashMap::new()).with_rule_store(Arc::new(StaticRuleStore {
            rules: vec![ActionPolicyRule {
                id: uuid::Uuid::now_v7(),
                scope: ActionRuleScope::Tenant {
                    tenant_id: session.tenant_id,
                },
                tool: "filtered_away".to_string(),
                pattern: "*".to_string(),
                effect: ActionPolicyEffect::Allow,
                reason: Some("rule for a tool that is not registered".to_string()),
                created_by: UserId::new("admin"),
                created_at: chrono::Utc::now(),
            }],
        }));

    assert!(!router.has_tool("filtered_away"));
    let error = router
        .check_policy(&session, &mcp_invocation("filtered_away"))
        .await
        .expect_err("an allow rule must not make an unregistered tool invocable");

    match error {
        MoaError::ToolError(message) => assert!(
            message.contains("unknown tool"),
            "expected an unknown-tool error, got: {message}"
        ),
        other => panic!("expected ToolError for an unregistered tool, got {other:?}"),
    }
}
