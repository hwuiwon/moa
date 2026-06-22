//! Restate service for workspace-scoped action-policy checks.

use std::sync::Arc;

use moa_core::{
    ActionEnvelope, ActionPolicyEffect, ActionPolicyRule, ActionReviewPreview, AgentPolicySnapshot,
    MoaError, SessionMeta, SubAgentId, ToolCallId, ToolInvocation,
};
use moa_hands::{ActionOrigin, ToolRouter};
use moa_security::stricter_effect;
use restate_sdk::prelude::*;
use uuid::Uuid;

use moa_core::restate_observability::annotate_restate_handler_span;

/// Request payload for `WorkspaceStore/prepare_action_review`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrepareActionReviewRequest {
    /// Session metadata used for workspace-scoped policy evaluation.
    pub session: SessionMeta,
    /// Tool invocation that is about to execute.
    pub invocation: ToolInvocation,
    /// Stable review identifier to embed in the envelope when review is needed.
    pub review_id: Uuid,
    /// Stable tool-call identifier for event correlation.
    pub tool_call_id: ToolCallId,
    /// Sub-agent that requested the action, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_agent_id: Option<SubAgentId>,
    /// Origin object kind for workflow or artifact-driven actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_kind: Option<String>,
    /// Origin object identifier for workflow or artifact-driven actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_id: Option<String>,
    /// Origin step identifier for workflow or artifact-driven actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_step_id: Option<String>,
    /// Explicit idempotency key supplied for side-effecting tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
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

/// Restate service surface for workspace-scoped action-policy operations.
#[restate_sdk::service]
pub trait WorkspaceStore {
    /// Evaluates policy for one tool invocation and prepares an action-review payload.
    async fn prepare_action_review(
        request: Json<PrepareActionReviewRequest>,
    ) -> Result<Json<PreparedActionReview>, HandlerError>;
}

/// Concrete Restate service implementation backed by the shared tool router.
#[derive(Clone)]
pub struct WorkspaceStoreImpl {
    router: Arc<ToolRouter>,
}

impl WorkspaceStoreImpl {
    /// Creates a new workspace-store facade backed by the shared router.
    #[must_use]
    pub fn new(router: Arc<ToolRouter>) -> Self {
        Self { router }
    }
}

impl WorkspaceStore for WorkspaceStoreImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal workflow call after the owning session or sub-agent has admitted the caller; user-facing review listing and decisions authorize in `ActionReviews`.
    async fn prepare_action_review(
        &self,
        ctx: Context<'_>,
        request: Json<PrepareActionReviewRequest>,
    ) -> Result<Json<PreparedActionReview>, HandlerError> {
        annotate_restate_handler_span("WorkspaceStore", "prepare_action_review");
        let request = request.into_inner();
        let router = self.router.clone();

        Ok(ctx
            .run(|| async move {
                let prepared = router
                    .prepare_invocation(&request.session, &request.invocation)
                    .await
                    .map_err(to_handler_error)?;
                let base_policy = prepared.policy().clone();
                let agent_policy = agent_action_policy_effect(
                    &request.session,
                    &request.invocation,
                    request.origin_kind.as_deref(),
                    request.origin_id.as_deref(),
                )
                .map_err(to_handler_error)?;
                let effect = stricter_effect(base_policy.effect, agent_policy.effect);
                let base_reason = base_policy.reason.clone();
                let reason = if effect == base_policy.effect {
                    base_reason.or(agent_policy.reason)
                } else {
                    agent_policy.reason.or(base_reason)
                };
                let origin = ActionOrigin {
                    origin_kind: request.origin_kind,
                    origin_id: request.origin_id,
                    origin_step_id: request.origin_step_id,
                    idempotency_key: request.idempotency_key,
                };
                Ok(Json::from(PreparedActionReview {
                    effect,
                    reason,
                    matched_rule: base_policy.matched_rule.clone(),
                    input_summary: prepared.input_summary().to_string(),
                    envelope: prepared.envelope(
                        request.review_id,
                        &request.session,
                        request.tool_call_id,
                        request.sub_agent_id,
                        origin,
                    ),
                    preview: prepared.review_preview(),
                }))
            })
            .name("prepare_action_review")
            .await?)
    }
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
        .map(moa_core::AgentContext::parsed_policy_snapshot)
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

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ActionPolicyEffect, AgentActionPolicy, AgentContext, AgentPolicySnapshot, AgentToolPolicy,
        AgentToolPolicyMode, SYSTEM_DEFAULT_AGENT_POLICY_HASH, SYSTEM_DEFAULT_AGENT_REF,
        SYSTEM_DEFAULT_AGENT_REVISION_UID, SessionMeta, ToolInvocation,
    };
    use serde_json::json;

    use super::{action_origin_ref, agent_action_policy_effect};

    #[test]
    fn agent_action_review_policy_upgrades_matching_action_to_review() {
        // Pins: agent action policy can require review for matching artifact-backed actions.
        let session = session_with_snapshot(AgentPolicySnapshot {
            action_policy: AgentActionPolicy {
                allowed: Vec::new(),
                require_admin_review: vec!["action://refund".to_string()],
            },
            ..AgentPolicySnapshot::default()
        });

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
        let session = session_with_snapshot(AgentPolicySnapshot {
            action_policy: AgentActionPolicy {
                allowed: vec!["action://refund".to_string()],
                require_admin_review: Vec::new(),
            },
            ..AgentPolicySnapshot::default()
        });

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
