//! Policy evaluation and action-review rendering for tool invocations.

use moa_core::{
    ActionClass, ActionEnvelope, ActionPolicyEffect, ActionPolicyRule, ActionReviewField,
    ActionReviewFileDiff, ActionReviewPreview, ActionRuleScope, IdempotencyClass, MoaError,
    ProcedureToolKind, Result, RiskLevel, SessionActorRef, SessionMeta, ToolCallId, ToolDefinition,
    ToolDiffStrategy, ToolInputShape, ToolInvocation, ToolPolicyInput, ToolPolicySpec, UserId,
    WorkerId, is_procedure_tool_name,
};
use serde_json::Value;
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
        worker_id: Option<WorkerId>,
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
            worker_id,
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
        // Workflow-owned procedure tools are not registered in the `ToolRegistry`
        // because they execute on the Restate workflow path rather than through a
        // hand/builtin/MCP executor. They still need a resolvable policy identity so
        // tenant tool-policy rules can match them; genuinely unknown tools continue
        // to fail closed.
        let registered = self.registry.get(&invocation.name);
        let synthetic_procedure_definition = if registered.is_none() {
            procedure_tool_definition(&invocation.name)
        } else {
            None
        };
        let tool_definition = match registered {
            Some(definition) => definition,
            None => synthetic_procedure_definition
                .as_ref()
                .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?,
        };
        let policy_input = self.describe_invocation(tool_definition, invocation)?;
        let rules = if let Some(rule_store) = &self.rule_store {
            let policy_actor = identity_actor_for_policy_lookup(session);
            rule_store
                .list_action_policy_rules_for_tool(
                    &session.tenant_id,
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
                .get(&session.tenant_id)
                .cloned()
                .or_else(|| self.sandbox_root.clone())
        } else {
            None
        };
        let action_pattern = if needs_review_preview {
            action_pattern_for(
                &invocation.name,
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

        let rule = ActionPolicyRule {
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
        };
        moa_security::validate_action_policy_rule(&rule)?;

        rule_store.upsert_action_policy_rule(rule).await
    }

    fn describe_invocation(
        &self,
        definition: &moa_core::ToolDefinition,
        invocation: &ToolInvocation,
    ) -> Result<ToolPolicyInput> {
        let normalized_input = normalized_input_for(
            &invocation.name,
            definition.policy.input_shape,
            &invocation.input,
        )?;
        Ok(ToolPolicyInput {
            tool_name: invocation.name.clone(),
            input_summary: summary_for(
                &invocation.name,
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

/// Builds a synthetic policy-only definition for a workflow-owned procedure tool.
///
/// `run_procedure`/`procedure_status` run on the Restate workflow path and are
/// deliberately absent from the [`ToolRegistry`]. Returning a definition here gives
/// the policy service a resolvable identity for them — defaulting to `Allow` so the
/// effective decision is unchanged unless a tenant rule matches — without registering
/// them as executable tools. Returns `None` for any other name so unregistered tools
/// still fail closed at the caller.
fn procedure_tool_definition(name: &str) -> Option<ToolDefinition> {
    if !is_procedure_tool_name(name) {
        return None;
    }
    let kind = ProcedureToolKind::from_name(name);
    let (risk_level, action_class, idempotency_class) = match kind {
        // Starting a run creates durable run state; individual side-effecting nodes
        // remain separately action-policy governed inside the procedure executor.
        // Classified NonIdempotent because the runtime cannot thread a durable
        // idempotency key through tool invocation and hands recovery, so an
        // automatic retry after uncertain execution could start a duplicate run.
        Some(ProcedureToolKind::Run) => (
            RiskLevel::Medium,
            ActionClass::LocalWrite,
            IdempotencyClass::NonIdempotent,
        ),
        // Polling only reads an existing run projection.
        _ => (
            RiskLevel::Low,
            ActionClass::Read,
            IdempotencyClass::Idempotent,
        ),
    };
    Some(ToolDefinition {
        name: name.to_string(),
        description: String::new(),
        schema: kind.map(ProcedureToolKind::schema).unwrap_or(Value::Null),
        policy: ToolPolicySpec {
            risk_level,
            default_effect: ActionPolicyEffect::Allow,
            action_class,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        },
        idempotency_class,
        // The policy path never persists procedure-tool output through this budget;
        // matches the shared default tool output budget for consistency.
        max_output_tokens: 8_000,
    })
}

fn identity_actor_for_policy_lookup(session: &SessionMeta) -> UserId {
    match &session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId(id.to_string()),
        _ => UserId(Uuid::nil().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use moa_core::{
        ActionPolicyEffect, ActionPolicyRule, ActionRuleScope, Result, SessionMeta, TenantId,
        ToolInvocation, UserId,
    };
    use moa_security::ActionPolicyRuleStore;
    use serde_json::json;
    use uuid::Uuid;

    use super::{ToolRouter, procedure_tool_definition};
    use crate::core::registration::ToolRegistry;

    fn session() -> SessionMeta {
        SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(7)),
            ..SessionMeta::default()
        }
    }

    fn run_procedure_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: "run_procedure".to_string(),
            input: json!({}),
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

    #[test]
    fn procedure_tools_resolve_to_allow_and_unknown_tools_do_not() {
        // Pins: workflow-owned procedure tools get a resolvable policy identity that
        // defaults to Allow, while any other unregistered tool stays unresolved so it
        // fails closed at the caller.
        let run = procedure_tool_definition("run_procedure").expect("run_procedure resolves");
        assert_eq!(run.policy.default_effect, ActionPolicyEffect::Allow);
        let status =
            procedure_tool_definition("procedure_status").expect("procedure_status resolves");
        assert_eq!(status.policy.default_effect, ActionPolicyEffect::Allow);
        assert!(procedure_tool_definition("bash").is_none());
        assert!(procedure_tool_definition("spawn_worker").is_none());
    }

    #[tokio::test]
    async fn check_policy_resolves_procedure_tools_but_rejects_unknown_tools() {
        // Pins: the policy service can evaluate a procedure tool that is absent from the
        // registry (default Allow), yet a genuinely unknown tool still errors, so the
        // default-deny posture for unregistered tools is preserved.
        let router = ToolRouter::new(ToolRegistry::new(), HashMap::new());
        let session = session();

        let allowed = router
            .check_policy(&session, &run_procedure_invocation())
            .await
            .expect("procedure tool resolves");
        assert_eq!(allowed.effect, ActionPolicyEffect::Allow);

        let unknown = router
            .check_policy(
                &session,
                &ToolInvocation {
                    id: None,
                    name: "not_a_real_tool".to_string(),
                    input: json!({}),
                },
            )
            .await;
        assert!(unknown.is_err(), "unknown tools remain unresolved");
    }

    #[tokio::test]
    async fn tenant_deny_rule_for_run_procedure_now_fires() {
        // Pins: because run_procedure is now policy-resolvable, a tenant Deny rule
        // targeting it is applied instead of being silently unreachable.
        let session = session();
        let deny_rule = ActionPolicyRule {
            id: Uuid::now_v7(),
            tool: "run_procedure".to_string(),
            pattern: "*".to_string(),
            effect: ActionPolicyEffect::Deny,
            scope: ActionRuleScope::Tenant {
                tenant_id: session.tenant_id,
            },
            reason: Some("procedures disabled for this tenant".to_string()),
            created_by: UserId::new("admin"),
            created_at: chrono::Utc::now(),
        };
        let router = ToolRouter::new(ToolRegistry::new(), HashMap::new()).with_rule_store(
            Arc::new(StaticRuleStore {
                rules: vec![deny_rule],
            }),
        );

        let decision = router
            .check_policy(&session, &run_procedure_invocation())
            .await
            .expect("procedure tool resolves");
        assert_eq!(decision.effect, ActionPolicyEffect::Deny);
    }
}
