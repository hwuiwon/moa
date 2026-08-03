//! Offline installed-connector overlay, provenance, policy, and recovery coverage.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::connector::ConnectorDefinition;
use moa_connectors::catalog::{InstalledConnectorCatalogQuery, InstalledConnectorCatalogSnapshot};
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth,
    ConnectionStatus, ConnectorConnection, InstalledActionBinding, InstalledActionBindingId,
};
use moa_connectors::executor::{
    ConnectorActionInvocation, ConnectorActionRuntime, RawConnectorActionResult,
};
use moa_core::traits::{BuiltInTool, Identity, IdentityType, ToolContext};
use moa_core::types::action_policy::{
    ActionClass, ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, RiskLevel,
};
use moa_core::types::agent::AgentConnectorBinding;
use moa_core::types::completion::ToolInvocation;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId, ToolCallId, UserId};
use moa_core::types::security::ToolCapabilityId;
use moa_core::types::session::SessionMeta;
use moa_core::types::tools::{
    ActionPolicyDecisionSource, IdempotencyClass, ToolDiffStrategy, ToolInputShape, ToolOutput,
    ToolPolicySpec,
};
use moa_hands::{
    PinnedToolOwner, ToolCallScope, ToolRegistry, ToolRouter, local_development_sandbox_policy,
};
use moa_security::ActionPolicyRuleStore;
use serde_json::{Value, json};
use uuid::Uuid;

const CONNECTOR_LOOKING_BASE_TOOL: &str = "conn__00000000000000000000000000000000__status";

struct ConnectorLookingBuiltIn;

#[async_trait]
impl BuiltInTool for ConnectorLookingBuiltIn {
    fn name(&self) -> &'static str {
        CONNECTOR_LOOKING_BASE_TOOL
    }

    fn description(&self) -> &'static str {
        "A base tool whose name deliberately resembles an installed connector action."
    }

    fn input_schema(&self) -> Value {
        json!({"type": "object"})
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
        _input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> moa_core::error::Result<ToolOutput> {
        Ok(ToolOutput::text("base", Duration::ZERO))
    }
}

struct CountingConnectorRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ConnectorActionRuntime for CountingConnectorRuntime {
    async fn invoke(
        &self,
        _invocation: ConnectorActionInvocation,
        _prepared: moa_connectors::executor::PreparedConnectorAction,
    ) -> moa_connectors::Result<RawConnectorActionResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(moa_connectors::Error::Http {
            code: "fixture_runtime_called",
        })
    }
}

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
    ) -> moa_core::error::Result<Vec<ActionPolicyRule>> {
        Ok(self
            .rules
            .iter()
            .filter(|rule| rule.tool == tool)
            .cloned()
            .collect())
    }

    async fn upsert_action_policy_rule(
        &self,
        _rule: ActionPolicyRule,
    ) -> moa_core::error::Result<()> {
        Ok(())
    }

    async fn delete_action_policy_rule(
        &self,
        _tenant_id: &TenantId,
        _user_id: Option<&UserId>,
        _tool: &str,
        _pattern: &str,
    ) -> moa_core::error::Result<()> {
        Ok(())
    }
}

#[test]
fn installed_overlay_keeps_typed_provenance_and_leaves_deployment_catalog_immutable_offline() {
    // Pins: installed actions receive exact typed catalog ownership while a base
    // tool with a connector-looking name remains a built-in; constructing the
    // per-agent overlay never publishes it into the deployment router.
    let fixture = Fixture::new();
    let router = router_with_base_tool();
    let base = router.activated_catalog();
    let base_pin = base.pin().expect("base catalog should pin");
    let runtime = Arc::new(CountingConnectorRuntime {
        calls: AtomicUsize::new(0),
    });
    let overlay = router
        .installed_connector_overlay(
            &base,
            &fixture.catalog,
            std::slice::from_ref(&fixture.agent_binding),
            runtime,
        )
        .expect("matching installed action and agent binding should overlay");
    let overlay_pin = overlay.pin().expect("overlay catalog should pin");

    assert_eq!(
        router.activated_catalog().pin().expect("live base pin"),
        base_pin
    );
    let base_owner = overlay_pin
        .tools
        .iter()
        .find(|tool| tool.tool == CONNECTOR_LOOKING_BASE_TOOL)
        .expect("connector-looking base tool should remain registered");
    assert_eq!(base_owner.owner, PinnedToolOwner::BuiltIn);

    let installed = overlay_pin
        .tools
        .iter()
        .find(|tool| tool.tool == fixture.tool_name)
        .expect("installed action should enter only the ephemeral overlay");
    assert_eq!(
        installed.owner,
        PinnedToolOwner::InstalledConnectorAction {
            connector_ref: fixture.agent_binding.connector_ref.clone(),
            connection_id: fixture.connection_id,
            binding_id: fixture.binding.binding_id.0,
            connection_generation: fixture.binding.connection_generation.get(),
            definition_artifact_uid: fixture.agent_binding.artifact_uid,
            definition_revision_uid: fixture.agent_binding.revision_uid,
            action_id: fixture.binding.action_id.clone(),
            contract_hash: fixture.binding.contract_hash.to_string(),
            governed_contract_revision: fixture.binding.governed_contract_revision.clone(),
            minimum_effect: ActionPolicyEffect::AdminReview,
        }
    );
    assert!(
        router.tool_definition(&fixture.tool_name).is_none(),
        "the global deployment catalog must not publish the per-agent action"
    );
    assert_eq!(
        overlay
            .tool_definition(&fixture.tool_name)
            .expect("overlay definition should be available")
            .description,
        "Connector action `create_invoice` using the selected connection \"Billing primary\"."
    );
}

#[tokio::test]
async fn installed_binding_minimum_effect_cannot_be_lifted_by_persisted_allow_offline() {
    // Pins: a persisted exact-operation Allow may lift an ordinary tool's
    // cautious default, but cannot lower an installed binding's AdminReview
    // minimum effect.
    let fixture = Fixture::new();
    let rule = ActionPolicyRule {
        id: Uuid::from_u128(0xa110),
        scope: ActionRuleScope::Tenant {
            tenant_id: fixture.tenant_id,
        },
        tool: fixture.tool_name.clone(),
        pattern: "*".to_string(),
        effect: ActionPolicyEffect::Allow,
        reason: Some("fixture tenant grant".to_string()),
        created_by: UserId("fixture-admin".to_string()),
        created_at: Utc::now(),
    };
    let router =
        router_with_base_tool().with_rule_store(Arc::new(StaticRuleStore { rules: vec![rule] }));
    let base = router.activated_catalog();
    let overlay = router
        .installed_connector_overlay(
            &base,
            &fixture.catalog,
            std::slice::from_ref(&fixture.agent_binding),
            Arc::new(CountingConnectorRuntime {
                calls: AtomicUsize::new(0),
            }),
        )
        .expect("fixture overlay should compile");
    let prepared = router
        .prepare_invocation_from_catalog(&overlay, &fixture.session(), &fixture.invocation())
        .await
        .expect("valid installed input should reach policy");

    assert_eq!(prepared.policy().effect, ActionPolicyEffect::AdminReview);
    assert_eq!(
        prepared.policy().source,
        ActionPolicyDecisionSource::ToolDefinition
    );
    assert_eq!(
        prepared
            .policy()
            .matched_rule
            .as_ref()
            .map(|rule| rule.effect),
        Some(ActionPolicyEffect::Allow),
        "the test must prove an Allow matched before the binding floor won"
    );
    assert_eq!(
        prepared.policy().reason.as_deref(),
        Some("the installed connector binding requires a stricter action-policy effect")
    );
}

#[tokio::test]
async fn generic_recovery_never_invokes_installed_connector_runtime_offline() {
    // Pins: connector replay and unknown-outcome decisions belong exclusively
    // to the connector invocation ledger. The generic hand/MCP recovery path
    // returns a classified failure without invoking the connector even once.
    let fixture = Fixture::new();
    let router = router_with_base_tool();
    let base = router.activated_catalog();
    let runtime = Arc::new(CountingConnectorRuntime {
        calls: AtomicUsize::new(0),
    });
    let overlay = router
        .installed_connector_overlay(
            &base,
            &fixture.catalog,
            std::slice::from_ref(&fixture.agent_binding),
            runtime.clone(),
        )
        .expect("fixture overlay should compile");
    let output = router
        .execute_authorized_with_recovery_from_catalog_within(
            &overlay,
            &fixture.session(),
            &fixture.identity,
            None,
            &fixture.invocation(),
            ToolCallId(Uuid::from_u128(0xc411)),
            None,
            ToolCallScope::unbounded(),
        )
        .await
        .expect("generic recovery should return a classified fail-closed result");

    assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
    assert!(output.safe_output.is_error);
    assert_eq!(
        output.capability,
        ToolCapabilityId::installed_connector_action(fixture.connection_id, "create_invoice")
    );
    assert_eq!(
        output.safe_output.to_text(),
        "installed connector actions require the durable pending-output dispatch path; generic recovery will not retransmit them"
    );
}

#[tokio::test]
async fn dedicated_pending_dispatch_invokes_connector_runtime_once_offline() {
    // Pins: the dedicated pending-output path is the sole hands entrypoint that
    // may invoke an installed connector, and one runtime failure is returned
    // after exactly one attempt rather than entering generic retry.
    let fixture = Fixture::new();
    let router = router_with_base_tool();
    let base = router.activated_catalog();
    let runtime = Arc::new(CountingConnectorRuntime {
        calls: AtomicUsize::new(0),
    });
    let overlay = router
        .installed_connector_overlay(
            &base,
            &fixture.catalog,
            std::slice::from_ref(&fixture.agent_binding),
            runtime.clone(),
        )
        .expect("fixture overlay should compile");
    let error = router
        .execute_installed_connector_pending_from_catalog_within(
            &overlay,
            &fixture.session(),
            &fixture.identity,
            &fixture.invocation(),
            ToolCallId(Uuid::from_u128(0xc412)),
            None,
            ToolCallScope::unbounded(),
        )
        .await
        .expect_err("fixture runtime should return its typed transport failure");

    assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(error, moa_core::error::MoaError::ToolError(message)
            if message == "connector HTTP operation failed at fixture_runtime_called")
    );
}

fn router_with_base_tool() -> ToolRouter {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(ConnectorLookingBuiltIn));
    ToolRouter::new(registry, HashMap::new(), local_development_sandbox_policy())
}

struct Fixture {
    tenant_id: TenantId,
    identity: Identity,
    connection_id: ConnectorConnectionId,
    binding: InstalledActionBinding,
    agent_binding: AgentConnectorBinding,
    catalog: InstalledConnectorCatalogSnapshot,
    tool_name: String,
}

impl Fixture {
    fn new() -> Self {
        let tenant_id = TenantId(Uuid::from_u128(0x7e11));
        let identity = Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(0x7e12),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        };
        let connection_id = ConnectorConnectionId(Uuid::from_u128(0xc011));
        let artifact_uid = Uuid::from_u128(0xa471);
        let revision_uid = Uuid::from_u128(0xa472);
        let generation =
            ConnectionGeneration::new(7).expect("fixture generation should be positive");
        let definition: ConnectorDefinition = serde_json::from_value(json!({
            "definition_version": "v1",
            "display_name": "Billing connector",
            "auth": [{"type": "none"}],
            "actions": [{
                "id": "create_invoice",
                "description": "Creates one invoice.",
                "contract": {
                    "method": "POST",
                    "path_template": "/invoices",
                    "max_request_bytes": 1024,
                    "max_response_bytes": 1024,
                    "connect_timeout_ms": 1000,
                    "total_timeout_ms": 2000,
                    "policy": {
                        "input_schema": {
                            "type": "object",
                            "properties": {"amount": {"type": "string"}},
                            "required": ["amount"],
                            "additionalProperties": false
                        },
                        "output_schema": {"type": "object"},
                        "data_classes": [],
                        "idempotency": "idempotent"
                    }
                }
            }]
        }))
        .expect("fixture connector definition should deserialize");
        let compiled_contract = CompiledOperationContract::compile(
            &definition,
            definition
                .actions
                .first()
                .expect("fixture should declare one action"),
        )
        .expect("fixture connector contract should compile");
        let contract_hash = compiled_contract
            .hash()
            .expect("fixture connector contract should hash");
        let binding = InstalledActionBinding {
            binding_id: InstalledActionBindingId(Uuid::from_u128(0xb111)),
            tenant_id,
            connection_id,
            connection_generation: generation,
            action_id: "create_invoice".to_string(),
            compiled_contract,
            contract_hash,
            governed_contract_revision: "connector-action/v1/create-invoice".to_string(),
            minimum_effect: ActionPolicyEffect::AdminReview,
            enabled: true,
        };
        let now = Utc::now();
        let connection = ConnectorConnection {
            connection_id,
            tenant_id,
            display_name: "Billing primary".to_string(),
            definition: ConnectionDefinitionRef::Artifact {
                artifact_uid,
                revision_uid,
            },
            origin: Some("https://api.example.test".parse().expect("fixture origin")),
            non_secret_config: json!({}),
            generation,
            status: ConnectionStatus::Active,
            health: ConnectionHealth::Ready,
            health_reason: None,
            created_by_identity_id: Some(identity.id),
            owner_identity_id: Some(identity.id),
            created_at: now,
            updated_at: now,
        };
        let query = InstalledConnectorCatalogQuery::new(identity.clone(), [connection_id]);
        let catalog = InstalledConnectorCatalogSnapshot::from_candidates(
            &query,
            [(connection, binding.clone())],
        )
        .expect("active current-generation fixture should enter the catalog");
        let agent_binding = AgentConnectorBinding {
            connector_ref: "connector://billing".to_string(),
            connection_id,
            artifact_uid,
            revision_uid,
        };
        let tool_name =
            moa_hands::core::installed_connector_tool_name(connection_id, "create_invoice")
                .expect("fixture action ID should produce a model-safe tool name");
        Self {
            tenant_id,
            identity,
            connection_id,
            binding,
            agent_binding,
            catalog,
            tool_name,
        }
    }

    fn session(&self) -> SessionMeta {
        SessionMeta {
            tenant_id: self.tenant_id,
            ..SessionMeta::default()
        }
    }

    fn invocation(&self) -> ToolInvocation {
        ToolInvocation {
            id: Some("connector-call".to_string()),
            name: self.tool_name.clone(),
            input: json!({"amount": "42.00"}),
        }
    }
}
