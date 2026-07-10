use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    ActionClass, ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, BuiltInTool,
    CloudHandsConfig, IdempotencyClass, MoaConfig, MoaError, ModelId, Result, RiskLevel,
    SessionMeta, TenantId, ToolContext, ToolDiffStrategy, ToolInputShape, ToolInvocation,
    ToolOutput, ToolPolicySpec, UserId,
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
        tenant_id: TenantId::new(),
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn local_config(allow_local_provider: bool) -> MoaConfig {
    let mut config = MoaConfig::default();
    config.local.docker_enabled = true;
    config.cloud.hands = Some(CloudHandsConfig {
        default_provider: Some("local".to_string()),
        allow_local_provider,
        ..CloudHandsConfig::default()
    });
    config
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
async fn local_route_without_opt_in_fails_closed() {
    // Pins: local hand routing is a development opt-in; local Docker support is
    // still the local provider and does not satisfy the enterprise sandbox tier.
    let mut config = local_config(false);
    let dir = tempdir().expect("tempdir should be created");
    config.local.sandbox_dir = dir.path().display().to_string();

    let error = match ToolRouter::from_config(&config).await {
        Ok(_) => panic!("local route without opt-in should fail closed"),
        Err(error) => error,
    };

    match error {
        MoaError::ConfigError(message) => {
            assert!(
                message.contains("MOA_CLOUD_HANDS_ALLOW_LOCAL"),
                "expected local opt-in env var in config error, got: {message}"
            );
            assert!(
                message.contains("local hand provider"),
                "expected local provider context in config error, got: {message}"
            );
        }
        other => panic!("expected ConfigError for local route without opt-in, got {other:?}"),
    }
}

#[tokio::test]
async fn local_route_with_opt_in_registers_local_hands() {
    // Pins: explicit development opt-in keeps the local hand provider usable for
    // offline development without re-enabling it implicitly for cloud defaults.
    let mut config = local_config(true);
    config.local.docker_enabled = false;
    let dir = tempdir().expect("tempdir should be created");
    config.local.sandbox_dir = dir.path().display().to_string();

    let router = ToolRouter::from_config(&config)
        .await
        .expect("local opt-in should allow router construction");
    assert!(router.has_tool("file_write"));
    assert!(router.has_tool("file_read"));
    let session = session();

    router
        .execute_authorized(
            &session,
            &ToolInvocation {
                id: None,
                name: "file_write".to_string(),
                input: json!({ "path": "notes.txt", "content": "allowed local opt-in" }),
            },
        )
        .await
        .expect("opted-in local file_write should execute");
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
        .expect("opted-in local file_read should execute");

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
                    &ToolInvocation {
                        id: None,
                        name: "secret_error".to_string(),
                        input,
                    },
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
            let mut config = local_config(true);
            config.local.docker_enabled = false;
            let dir = tempdir().expect("tempdir should be created");
            config.local.sandbox_dir = dir.path().display().to_string();
            let router = ToolRouter::from_config(&config)
                .await
                .expect("local opt-in should allow router construction");
            let (_, output) = router
                .execute_authorized(
                    &session(),
                    &ToolInvocation {
                        id: None,
                        name: "bash".to_string(),
                        input,
                    },
                )
                .await
                .expect("failing bash command should still return a ToolOutput");

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
async fn mcp_tool_defaults_to_admin_review() {
    // Pins: an MCP/third-party tool has no considered per-tool descriptor gate, so it
    // resolves to AdminReview by default instead of a bare allow — unvetted external
    // code must not execute unattended.
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool("external-server", discovered_mcp_tool("external_action"))
        .expect("register mcp tool");
    let router = ToolRouter::new(registry, HashMap::new());

    let check = router
        .check_policy(&session(), &mcp_invocation("external_action"))
        .await
        .expect("policy check for mcp tool");

    assert_eq!(check.effect, ActionPolicyEffect::AdminReview);
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
async fn explicitly_allowed_mcp_tool_resolves_to_allow() {
    // Pins: an explicit operator allow rule overrides the MCP admin-review default, so
    // operator config still wins over the new secure default.
    let session = session();
    let mut registry = ToolRegistry::new();
    registry
        .register_mcp_tool("external-server", discovered_mcp_tool("external_action"))
        .expect("register mcp tool");
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
    let router =
        ToolRouter::new(registry, HashMap::new()).with_rule_store(Arc::new(StaticRuleStore {
            rules: vec![allow_rule],
        }));

    let check = router
        .check_policy(&session, &mcp_invocation("external_action"))
        .await
        .expect("policy check for allowed mcp tool");

    assert_eq!(check.effect, ActionPolicyEffect::Allow);
}
