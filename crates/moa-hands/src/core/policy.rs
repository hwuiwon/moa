//! Policy evaluation and action-review rendering for tool invocations.

use moa_core::{
    ActionEnvelope, ActionPolicyEffect, ActionPolicyRule, ActionReviewField, ActionReviewFileDiff,
    ActionReviewPreview, ActionRuleScope, MoaError, Result, SessionActorRef, SessionMeta,
    SubAgentId, ToolCallId, ToolInvocation, ToolPolicyInput, UserId, WorkspaceId,
};
use uuid::Uuid;

use super::ToolRouter;
use super::normalization::{
    action_pattern_for, normalized_input_for, review_diffs_for, review_fields_for, summary_for,
};

/// Optional origin metadata attached to an action envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionOrigin {
    /// Origin object kind for workflow or artifact-driven actions.
    pub origin_kind: Option<String>,
    /// Origin object identifier for workflow or artifact-driven actions.
    pub origin_id: Option<String>,
    /// Origin step identifier for workflow or artifact-driven actions.
    pub origin_step_id: Option<String>,
    /// Explicit idempotency key supplied for side-effecting tools.
    pub idempotency_key: Option<String>,
}

/// Prepared metadata for a concrete tool invocation.
#[derive(Debug, Clone)]
pub struct PreparedActionInvocation {
    /// Normalized policy-facing description of the invocation.
    policy_input: ToolPolicyInput,
    /// Result of evaluating the invocation against the active policies.
    policy: moa_security::ActionPolicyCheck,
    /// Suggested rule pattern for future action-policy matching.
    action_pattern: String,
    /// Structured review fields for the local UI.
    review_fields: Vec<ActionReviewField>,
    /// Optional inline file diffs for the local UI.
    review_diffs: Vec<ActionReviewFileDiff>,
}

impl PreparedActionInvocation {
    /// Returns the policy evaluation outcome for the invocation.
    pub fn policy(&self) -> &moa_security::ActionPolicyCheck {
        &self.policy
    }

    /// Returns the normalized policy input used for rule evaluation.
    pub fn policy_input(&self) -> &ToolPolicyInput {
        &self.policy_input
    }

    /// Returns the concise invocation summary for tool cards and errors.
    pub fn input_summary(&self) -> &str {
        &self.policy_input.input_summary
    }

    /// Builds the durable action envelope for this invocation.
    pub fn envelope(
        &self,
        review_id: Uuid,
        session: &SessionMeta,
        tool_call_id: ToolCallId,
        sub_agent_id: Option<SubAgentId>,
        origin: ActionOrigin,
    ) -> ActionEnvelope {
        ActionEnvelope {
            review_id,
            tenant_id: session.tenant_id,
            requested_by: session
                .created_by
                .clone()
                .unwrap_or(SessionActorRef::Anonymous),
            session_id: Some(session.id),
            sub_agent_id,
            tool_call_id,
            tool_name: self.policy_input.tool_name.clone(),
            normalized_input: self.policy_input.normalized_input.clone(),
            input_summary: self.policy_input.input_summary.clone(),
            risk_level: self.policy_input.risk_level,
            action_class: self.policy_input.action_class,
            origin_kind: origin.origin_kind,
            origin_id: origin.origin_id,
            origin_step_id: origin.origin_step_id,
            idempotency_key: origin.idempotency_key,
            created_at: chrono::Utc::now(),
        }
    }

    /// Builds the action-review preview for this invocation.
    pub fn review_preview(&self) -> ActionReviewPreview {
        let mut fields = self.review_fields.clone();
        fields.push(ActionReviewField {
            label: "Action pattern".to_string(),
            value: self.action_pattern.clone(),
        });
        ActionReviewPreview {
            fields,
            file_diffs: self.review_diffs.clone(),
        }
    }
}

impl ToolRouter {
    /// Evaluates the policy effect for a tool invocation in the current session.
    pub async fn check_policy(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<moa_security::ActionPolicyCheck> {
        Ok(self
            .prepare_invocation(session, invocation)
            .await?
            .policy()
            .clone())
    }

    /// Prepares a tool invocation for policy evaluation and action-review rendering.
    pub async fn prepare_invocation(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PreparedActionInvocation> {
        let tool_definition = self
            .registry
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;
        let policy_input = self.describe_invocation(tool_definition, invocation)?;
        let rules = if let Some(rule_store) = &self.rule_store {
            let policy_workspace_key = tenant_workspace_key(session);
            let policy_actor = identity_actor_for_policy_lookup(session);
            rule_store
                .list_action_policy_rules_for_tool(
                    &policy_workspace_key,
                    &policy_actor,
                    &invocation.name,
                )
                .await?
        } else {
            Vec::new()
        };
        let policy = self.policies.check(
            &policy_input,
            &moa_security::ActionPolicyContext::from_session(session),
            &rules,
        )?;
        let needs_review_preview = matches!(policy.effect, ActionPolicyEffect::AdminReview);
        let review_root = if needs_review_preview {
            self.workspace_roots
                .read()
                .await
                .get(&tenant_workspace_key(session))
                .cloned()
                .or_else(|| self.sandbox_root.clone())
        } else {
            None
        };
        let action_pattern = if needs_review_preview {
            action_pattern_for(
                tool_definition.policy.input_shape,
                &policy_input.normalized_input,
            )
        } else {
            String::new()
        };
        let review_fields = if needs_review_preview {
            review_fields_for(
                review_root.as_deref(),
                tool_definition.policy.input_shape,
                invocation,
            )
        } else {
            Vec::new()
        };
        let review_diffs = if needs_review_preview {
            review_diffs_for(
                review_root.as_deref(),
                tool_definition.policy.diff_strategy,
                invocation,
            )
            .await?
        } else {
            Vec::new()
        };

        Ok(PreparedActionInvocation {
            action_pattern,
            review_fields,
            review_diffs,
            policy_input,
            policy,
        })
    }

    /// Persists an action-policy rule for the current workspace.
    pub async fn store_action_policy_rule(
        &self,
        session: &SessionMeta,
        tool: &str,
        pattern: &str,
        effect: ActionPolicyEffect,
        created_by: UserId,
    ) -> Result<()> {
        let Some(rule_store) = &self.rule_store else {
            return Err(MoaError::Unsupported(
                "tool router does not have an action-policy rule store".to_string(),
            ));
        };

        rule_store
            .upsert_action_policy_rule(ActionPolicyRule {
                id: Uuid::now_v7(),
                tool: tool.to_string(),
                pattern: pattern.to_string(),
                effect,
                scope: ActionRuleScope::Tenant {
                    tenant_id: session.tenant_id,
                },
                reason: None,
                created_by,
                created_at: chrono::Utc::now(),
            })
            .await
    }

    fn describe_invocation(
        &self,
        definition: &moa_core::ToolDefinition,
        invocation: &ToolInvocation,
    ) -> Result<ToolPolicyInput> {
        let normalized_input =
            normalized_input_for(definition.policy.input_shape, &invocation.input)?;
        Ok(ToolPolicyInput {
            tool_name: invocation.name.clone(),
            input_summary: summary_for(
                definition.policy.input_shape,
                &invocation.input,
                &normalized_input,
            ),
            normalized_input,
            risk_level: definition.policy.risk_level,
            default_effect: definition.policy.default_effect,
            action_class: definition.policy.action_class,
        })
    }
}

fn tenant_workspace_key(session: &SessionMeta) -> WorkspaceId {
    WorkspaceId::new(session.tenant_id.to_string())
}

fn identity_actor_for_policy_lookup(session: &SessionMeta) -> UserId {
    match &session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId(id.to_string()),
        _ => UserId(Uuid::nil().to_string()),
    }
}
