//! Restate service for tenant action-policy checks.

use std::sync::Arc;

use chrono::Utc;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::{
    error::MoaError, types::action_policy::ActionEnvelope,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionPolicyRule,
    types::action_policy::ActionReviewOwner, types::action_policy::ActionReviewPreview,
    types::action_policy::ActionRuleScope, types::action_policy::CapabilityProvenance,
    types::agent::AgentPolicySnapshot, types::completion::ToolInvocation,
    types::contact::ContactId, types::identifiers::TenantId, types::identifiers::ToolCallId,
    types::identifiers::UserId, types::session::SessionMeta,
};
use moa_hands::{ActionOrigin, ToolRouter};
use moa_security::{ActionPolicyRuleStore, stricter_effect};
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::workflows::errors::moa_error_to_handler_error;
use moa_observability::restate_observability::annotate_restate_handler_span;

/// Request payload for `ActionPolicy/prepare_action_review`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrepareActionReviewRequest {
    /// Session metadata used for tenant-scoped policy evaluation.
    pub session: SessionMeta,
    /// Tool invocation that is about to execute.
    pub invocation: ToolInvocation,
    /// Stable review identifier to embed in the envelope when review is needed.
    pub review_id: Uuid,
    /// Stable tool-call identifier for event correlation.
    pub tool_call_id: ToolCallId,
    /// Exact owner resumed if this action is queued for review. Required.
    pub owner: ActionReviewOwner,
    /// Capability-level provenance, independent of execution ownership.
    #[serde(default)]
    pub capability_provenance: CapabilityProvenance,
    /// Explicit idempotency key supplied for side-effecting tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Request payload for `ActionPolicy/upsert_rule`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpsertActionPolicyRuleRequest {
    /// Tenant that owns the rule.
    pub tenant_id: TenantId,
    /// Contact that owns the rule when creating a personal/contact-scoped override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact_id: Option<ContactId>,
    /// Tool name the rule applies to.
    pub tool_name: String,
    /// Persisted normalized pattern.
    pub pattern: String,
    /// Effect applied when the rule matches.
    pub effect: ActionPolicyEffect,
    /// Optional reason stored with the rule.
    pub reason: Option<String>,
}

/// Prepared policy decision and review payload for one tool call.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreparedActionReview {
    /// Final policy effect for this invocation.
    pub effect: ActionPolicyEffect,
    /// Optional human-readable reason for the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Matching action-policy rule when the decision came from persisted policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_rule: Option<ActionPolicyRule>,
    /// Human-readable invocation summary.
    pub input_summary: String,
    /// Durable action envelope for review/audit.
    pub envelope: ActionEnvelope,
    /// Human-readable action-review preview.
    pub preview: ActionReviewPreview,
}

/// Outcome of preparing one tool invocation for action review.
///
/// Model-authored input that fails schema validation is a conversation-level
/// mistake the model can correct, so it is returned as a value instead of a
/// handler error (which Restate would retry even though the failure is
/// deterministic).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PreparedActionReviewResponse {
    /// Policy evaluation completed; carry the prepared review.
    Prepared(Box<PreparedActionReview>),
    /// The invocation input failed schema validation.
    InvalidInput {
        /// Human-readable validation failure to surface to the model.
        reason: String,
    },
}

/// Restate service surface for tenant-scoped action-policy operations.
#[restate_sdk::service]
pub trait ActionPolicy {
    /// Evaluates policy for one tool invocation and prepares an action-review payload.
    async fn prepare_action_review(
        request: Json<PrepareActionReviewRequest>,
    ) -> Result<Json<PreparedActionReviewResponse>, HandlerError>;

    /// Creates or updates a tenant action-policy rule in the authoritative store.
    async fn upsert_rule(request: Json<UpsertActionPolicyRuleRequest>) -> Result<(), HandlerError>;
}

/// Concrete Restate service implementation backed by the shared tool router.
#[derive(Clone)]
pub struct ActionPolicyImpl {
    router: Arc<ToolRouter>,
    rule_store: Arc<dyn ActionPolicyRuleStore>,
}

impl ActionPolicyImpl {
    /// Creates a new action-policy facade backed by the shared router.
    #[must_use]
    pub fn new(router: Arc<ToolRouter>, rule_store: Arc<dyn ActionPolicyRuleStore>) -> Self {
        Self { router, rule_store }
    }
}

impl ActionPolicy for ActionPolicyImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal workflow call after the owning session or worker has admitted the caller; user-facing review listing and decisions authorize in `ActionReviews`.
    async fn prepare_action_review(
        &self,
        ctx: Context<'_>,
        request: Json<PrepareActionReviewRequest>,
    ) -> Result<Json<PreparedActionReviewResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionPolicy", "prepare_action_review");
        let request = request.into_inner();
        let router = self.router.clone();

        Ok(ctx
            .run(|| async move {
                prepare_action_review_inner(router, request)
                    .await
                    .map(Json::from)
            })
            .name("prepare_action_review")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn upsert_rule(
        &self,
        ctx: Context<'_>,
        request: Json<UpsertActionPolicyRuleRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionPolicy", "upsert_rule");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_tenant_admin(&identity, request.tenant_id).await?;
        let created_by = UserId::new(identity.id.to_string());
        let rule_store = self.rule_store.clone();

        Ok(ctx
            .run(|| async move {
                let scope = match request.contact_id {
                    Some(contact_id) => ActionRuleScope::Contact {
                        tenant_id: request.tenant_id,
                        contact_id,
                    },
                    None => ActionRuleScope::Tenant {
                        tenant_id: request.tenant_id,
                    },
                };
                let rule = ActionPolicyRule {
                    id: Uuid::now_v7(),
                    tool: request.tool_name,
                    pattern: request.pattern,
                    effect: request.effect,
                    scope,
                    reason: request.reason,
                    created_by,
                    created_at: Utc::now(),
                };
                rule_store
                    .upsert_action_policy_rule(rule)
                    .await
                    .map_err(moa_error_to_handler_error)
            })
            .name("action_policy_upsert_rule")
            .await?)
    }
}

async fn prepare_action_review_inner(
    router: Arc<ToolRouter>,
    request: PrepareActionReviewRequest,
) -> Result<PreparedActionReviewResponse, HandlerError> {
    let prepared = match router
        .prepare_invocation(&request.session, &request.invocation)
        .await
    {
        Ok(prepared) => prepared,
        Err(MoaError::ValidationError(reason)) => {
            return Ok(PreparedActionReviewResponse::InvalidInput { reason });
        }
        Err(error) => return Err(moa_error_to_handler_error(error)),
    };
    let base_policy = prepared.policy().clone();
    let agent_policy = agent_action_policy_effect(
        &request.session,
        &request.invocation,
        request.capability_provenance.kind.as_deref(),
        request.capability_provenance.id.as_deref(),
    )
    .map_err(moa_error_to_handler_error)?;
    let effect = stricter_effect(base_policy.effect, agent_policy.effect);
    let base_reason = base_policy.reason.clone();
    let reason = if effect == base_policy.effect {
        base_reason.or(agent_policy.reason)
    } else {
        agent_policy.reason.or(base_reason)
    };
    let origin = ActionOrigin {
        capability: request.capability_provenance,
        idempotency_key: request.idempotency_key,
    };
    Ok(PreparedActionReviewResponse::Prepared(Box::new(
        PreparedActionReview {
            effect,
            reason,
            matched_rule: base_policy.matched_rule.clone(),
            input_summary: prepared.input_summary().to_string(),
            envelope: prepared.envelope(
                request.review_id,
                &request.session,
                request.tool_call_id,
                request.owner,
                origin,
            ),
            preview: prepared.review_preview(),
        },
    )))
}

async fn require_tenant_admin(
    identity: &moa_core::traits::Identity,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentActionPolicyDecision {
    effect: ActionPolicyEffect,
    reason: Option<String>,
}

fn agent_action_policy_effect(
    session: &SessionMeta,
    invocation: &ToolInvocation,
    origin_kind: Option<&str>,
    origin_id: Option<&str>,
) -> Result<AgentActionPolicyDecision, MoaError> {
    let Some(snapshot) = agent_policy_snapshot(session)? else {
        return Ok(allow_agent_action());
    };

    if !snapshot.tool_policy.allows(&invocation.name) {
        return Ok(AgentActionPolicyDecision {
            effect: ActionPolicyEffect::Deny,
            reason: Some(format!(
                "tool `{}` is denied by the configured agent tool policy",
                invocation.name
            )),
        });
    }

    if let Some(action_ref) = action_origin_ref(origin_kind, origin_id) {
        if !snapshot.action_policy.allowed.is_empty()
            && !snapshot
                .action_policy
                .allowed
                .iter()
                .any(|allowed| allowed == &action_ref)
        {
            return Ok(AgentActionPolicyDecision {
                effect: ActionPolicyEffect::Deny,
                reason: Some(format!(
                    "action `{action_ref}` is outside the configured agent action allowlist"
                )),
            });
        }
        if snapshot
            .action_policy
            .require_admin_review
            .iter()
            .any(|required| required == &action_ref)
        {
            return Ok(AgentActionPolicyDecision {
                effect: ActionPolicyEffect::AdminReview,
                reason: Some(format!(
                    "action `{action_ref}` requires review by configured agent policy"
                )),
            });
        }
    }

    Ok(allow_agent_action())
}

fn agent_policy_snapshot(session: &SessionMeta) -> Result<Option<AgentPolicySnapshot>, MoaError> {
    session
        .agent_context
        .as_ref()
        .map(moa_core::types::agent::AgentContext::parsed_policy_snapshot)
        .transpose()
}

fn action_origin_ref(origin_kind: Option<&str>, origin_id: Option<&str>) -> Option<String> {
    let origin_kind = origin_kind?;
    let origin_id = origin_id?.trim();
    if origin_id.is_empty() {
        return None;
    }
    match origin_kind {
        "action" | "artifact_action" | "workflow_action" => {
            if origin_id.starts_with("action://") {
                Some(origin_id.to_string())
            } else {
                Some(format!("action://{origin_id}"))
            }
        }
        _ => None,
    }
}

fn allow_agent_action() -> AgentActionPolicyDecision {
    AgentActionPolicyDecision {
        effect: ActionPolicyEffect::Allow,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use moa_core::{
        types::action_policy::ActionPolicyEffect, types::action_policy::ActionReviewOwner,
        types::action_policy::CapabilityProvenance, types::action_policy::ExecutionTaskOrigin,
        types::agent::AgentActionPolicy, types::agent::AgentContext,
        types::agent::AgentPolicySnapshot, types::agent::AgentToolPolicy,
        types::agent::AgentToolPolicyMode, types::agent::SYSTEM_DEFAULT_AGENT_POLICY_HASH,
        types::agent::SYSTEM_DEFAULT_AGENT_REF, types::agent::SYSTEM_DEFAULT_AGENT_REVISION_UID,
        types::completion::ToolInvocation, types::identifiers::ToolCallId,
        types::session::SessionMeta,
    };
    use moa_hands::{McpDiscoveredTool, ToolRegistry, ToolRouter};
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        PrepareActionReviewRequest, PreparedActionReviewResponse, action_origin_ref,
        agent_action_policy_effect, prepare_action_review_inner,
    };

    #[tokio::test]
    async fn invalid_model_authored_tool_input_returns_value_not_handler_error() {
        // Pins: schema-invalid tool input from the model comes back as the
        // InvalidInput response value (so the turn can hand the model a
        // correctable tool error) instead of a handler error that Restate
        // retries forever on a deterministic failure.
        let mut registry = ToolRegistry::default_local();
        registry
            .register_mcp_tool(
                "github",
                McpDiscoveredTool {
                    name: "github_issue_create".to_string(),
                    description: "create an issue".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "required": ["title"],
                        "properties": {"title": {"type": "string"}}
                    }),
                },
            )
            .expect("MCP fixture should register");
        let github_issue_create = moa_hands::mcp_tool_reference("github", "github_issue_create");
        let router = Arc::new(ToolRouter::new(
            registry,
            HashMap::new(),
            moa_hands::local_development_sandbox_policy(),
        ));
        let request = policy_request(
            SessionMeta::default(),
            ToolInvocation {
                id: Some("invalid-input-call".to_string()),
                name: github_issue_create.clone(),
                input: json!({"title": 7}),
            },
            None,
        );

        let response = prepare_action_review_inner(router, request)
            .await
            .expect("validation failure must not be a handler error");
        let PreparedActionReviewResponse::InvalidInput { reason } = response else {
            panic!("expected InvalidInput, got {response:?}");
        };
        assert!(reason.contains(&github_issue_create), "reason: {reason}");
    }

    #[tokio::test]
    async fn action_policy_root_and_execution_origins_have_exact_effect_parity() {
        // Pins: every execution-backed policy class uses the same production preparation helper.
        let mut registry = ToolRegistry::default_local();
        registry
            .register_mcp_tool(
                "github",
                McpDiscoveredTool {
                    name: "github_issue_create".to_string(),
                    description: "create an issue".to_string(),
                    input_schema: json!({"type": "object"}),
                },
            )
            .expect("MCP fixture should register");
        let github_issue_create = moa_hands::mcp_tool_reference("github", "github_issue_create");
        let router = Arc::new(ToolRouter::new(
            registry,
            HashMap::new(),
            moa_hands::local_development_sandbox_policy(),
        ));
        let cases = [
            (
                "read",
                "file_read",
                json!({"path": "README.md"}),
                ActionPolicyEffect::Allow,
                false,
            ),
            (
                "local_write",
                "file_write",
                json!({"path": "notes.txt", "content": "hello"}),
                ActionPolicyEffect::Allow,
                false,
            ),
            (
                "command",
                "bash",
                json!({"cmd": "true"}),
                ActionPolicyEffect::AdminReview,
                false,
            ),
            (
                "external_mcp",
                github_issue_create.as_str(),
                json!({"title": "issue"}),
                ActionPolicyEffect::AdminReview,
                false,
            ),
            (
                "memory_read",
                "memory_search",
                json!({"query": "decision"}),
                ActionPolicyEffect::Allow,
                false,
            ),
            (
                "memory_write",
                "memory_remember",
                json!({"items": [{"text": "decision"}]}),
                ActionPolicyEffect::Allow,
                false,
            ),
            (
                "agent_deny",
                "file_read",
                json!({"path": "README.md"}),
                ActionPolicyEffect::Deny,
                true,
            ),
        ];

        for (label, tool_name, input, expected, denied_by_agent) in cases {
            let session = if denied_by_agent {
                session_with_snapshot(AgentPolicySnapshot {
                    tool_policy: AgentToolPolicy {
                        mode: AgentToolPolicyMode::Allowlist,
                        tools: vec!["memory_search".to_string()],
                        denied_tools: Vec::new(),
                    },
                    ..AgentPolicySnapshot::default()
                })
            } else {
                SessionMeta::default()
            };
            let invocation = ToolInvocation {
                id: Some(format!("{label}-call")),
                name: tool_name.to_string(),
                input,
            };
            let root = policy_request(session.clone(), invocation.clone(), None);
            let execution = policy_request(
                session,
                invocation,
                Some(ExecutionTaskOrigin {
                    run_uid: Uuid::from_u128(10),
                    task_uid: Uuid::from_u128(20),
                    generation: 3,
                }),
            );

            let root = prepare_action_review_inner(router.clone(), root)
                .await
                .unwrap_or_else(|error| panic!("{label} root preparation failed: {error:?}"));
            let execution = prepare_action_review_inner(router.clone(), execution)
                .await
                .unwrap_or_else(|error| panic!("{label} execution preparation failed: {error:?}"));
            let PreparedActionReviewResponse::Prepared(root) = root else {
                panic!("{label} root preparation rejected input");
            };
            let PreparedActionReviewResponse::Prepared(execution) = execution else {
                panic!("{label} execution preparation rejected input");
            };

            assert_eq!(root.effect, expected, "{label} root effect changed");
            assert_eq!(
                execution.effect, expected,
                "{label} execution effect changed"
            );
            assert_eq!(root.effect, execution.effect, "{label} parity changed");
            assert_eq!(root.envelope.owner.execution_origin(), None);
            assert!(execution.envelope.owner.execution_origin().is_some());
            assert!(root.envelope.owner.is_conversational());
            assert!(!execution.envelope.owner.is_conversational());
            assert_eq!(
                root.envelope.normalized_input, execution.envelope.normalized_input,
                "{label} normalization must be origin-independent"
            );
        }
    }

    fn policy_request(
        session: SessionMeta,
        invocation: ToolInvocation,
        execution_origin: Option<ExecutionTaskOrigin>,
    ) -> PrepareActionReviewRequest {
        let session_id = session.id;
        let owner = match execution_origin {
            Some(origin) => ActionReviewOwner::ExecutionTask { session_id, origin },
            None => ActionReviewOwner::Coordinator {
                session_id,
                turn_id: "turn-policy-fixture".to_string(),
                generation: 1,
            },
        };
        PrepareActionReviewRequest {
            session,
            invocation,
            review_id: Uuid::now_v7(),
            tool_call_id: ToolCallId::new(),
            owner,
            capability_provenance: CapabilityProvenance::default(),
            idempotency_key: None,
        }
    }

    #[test]
    fn agent_action_review_policy_upgrades_matching_action_to_review() {
        // Pins: agent action policy can require review for matching artifact-backed actions.
        let session = session_with_snapshot(snapshot_with_action_rules(
            Vec::new(),
            vec!["action://refund".to_string()],
        ));

        let decision = agent_action_policy_effect(
            &session,
            &invocation("bash"),
            Some("action"),
            Some("refund"),
        )
        .expect("policy");

        assert_eq!(decision.effect, ActionPolicyEffect::AdminReview);
    }

    #[test]
    fn agent_tool_policy_denies_disallowed_raw_tool() {
        // Pins: raw built-in and MCP tools remain governed by configured-agent tool policy.
        let session = session_with_snapshot(AgentPolicySnapshot {
            tool_policy: AgentToolPolicy {
                mode: AgentToolPolicyMode::Allowlist,
                tools: vec!["file_read".to_string()],
                denied_tools: Vec::new(),
            },
            ..AgentPolicySnapshot::default()
        });

        let decision =
            agent_action_policy_effect(&session, &invocation("bash"), None, None).expect("policy");

        assert_eq!(decision.effect, ActionPolicyEffect::Deny);
    }

    #[test]
    fn agent_action_allowlist_only_gates_action_origins() {
        // Pins: action allowlists apply to artifact-backed action origins, not raw tools.
        let session = session_with_snapshot(snapshot_with_action_rules(
            vec!["action://refund".to_string()],
            Vec::new(),
        ));

        let raw_tool =
            agent_action_policy_effect(&session, &invocation("bash"), None, None).expect("raw");
        let wrong_action = agent_action_policy_effect(
            &session,
            &invocation("bash"),
            Some("action"),
            Some("chargeback"),
        )
        .expect("action");

        assert_eq!(raw_tool.effect, ActionPolicyEffect::Allow);
        assert_eq!(wrong_action.effect, ActionPolicyEffect::Deny);
        assert_eq!(
            action_origin_ref(Some("action"), Some("refund")).as_deref(),
            Some("action://refund")
        );
    }

    fn invocation(name: &str) -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: name.to_string(),
            input: json!({}),
        }
    }

    fn snapshot_with_action_rules(
        allowed: Vec<String>,
        require_admin_review: Vec<String>,
    ) -> AgentPolicySnapshot {
        AgentPolicySnapshot {
            action_policy: AgentActionPolicy {
                allowed,
                require_admin_review,
            },
            ..AgentPolicySnapshot::default()
        }
    }

    fn session_with_snapshot(snapshot: AgentPolicySnapshot) -> SessionMeta {
        SessionMeta {
            agent_context: Some(AgentContext {
                agent_id: None,
                installation_uid: None,
                deployment_uid: None,
                definition_ref: SYSTEM_DEFAULT_AGENT_REF.to_string(),
                revision_uid: SYSTEM_DEFAULT_AGENT_REVISION_UID,
                policy_hash: SYSTEM_DEFAULT_AGENT_POLICY_HASH.to_string(),
                display_name: "Test Agent".to_string(),
                artifact_dependencies: Vec::new(),
                tool_dependencies: Vec::new(),
                policy_snapshot: serde_json::to_value(snapshot).expect("serialize snapshot"),
            }),
            ..SessionMeta::default()
        }
    }
}
