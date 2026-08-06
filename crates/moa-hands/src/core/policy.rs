//! Policy evaluation and action-review rendering for tool invocations.

use std::error::Error as StdError;

use jsonschema::{Draft, Retrieve, Uri};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionEnvelope,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionReviewField,
    types::action_policy::ActionReviewFileDiff, types::action_policy::ActionReviewOwner,
    types::action_policy::ActionReviewPreview, types::action_policy::CapabilityProvenance,
    types::completion::ToolInvocation, types::contact::SessionActorRef,
    types::identifiers::ToolCallId, types::identifiers::UserId, types::session::SessionMeta,
    types::tools::ActionPolicyDecisionSource, types::tools::ToolPolicyInput,
};
use serde_json::Value;
use uuid::Uuid;

use super::normalization::{
    action_pattern_for, normalized_input_for, review_diffs_for, review_fields_for, summary_for,
};
use super::{ToolCatalogSnapshot, ToolRouter};

/// Optional origin metadata attached to an action envelope.
///
/// Capability provenance describes which artifact or skill surface produced the
/// call. It is deliberately independent of the envelope's
/// [`ActionReviewOwner`], which decides who is resumed when the review resolves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionOrigin {
    /// Capability-level source provenance.
    pub capability: CapabilityProvenance,
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
    ///
    /// `owner` is the exact runtime resumed when the review resolves; it is
    /// supplied by the caller that issued the tool call and is never derived from
    /// the session metadata.
    pub fn envelope(
        &self,
        review_id: Uuid,
        session: &SessionMeta,
        tool_call_id: ToolCallId,
        owner: ActionReviewOwner,
        origin: ActionOrigin,
    ) -> ActionEnvelope {
        ActionEnvelope {
            review_id,
            tenant_id: session.tenant_id,
            requested_by: session
                .created_by
                .clone()
                .unwrap_or(SessionActorRef::Anonymous),
            owner,
            tool_call_id,
            tool_name: self.policy_input.tool_name.clone(),
            normalized_input: self.policy_input.normalized_input.clone(),
            input_summary: self.policy_input.input_summary.clone(),
            risk_level: self.policy_input.risk_level,
            action_class: self.policy_input.action_class,
            origin_kind: origin.capability.kind,
            origin_id: origin.capability.id,
            origin_step_id: origin.capability.step_id,
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
        let catalog = self.activated_catalog();
        self.prepare_invocation_from_catalog(&catalog, session, invocation)
            .await
    }

    /// Prepares policy from one caller-selected immutable catalog publication.
    ///
    /// Keeping the snapshot explicit lets prompt compilation, policy, retry
    /// metadata, and dispatch all use the same governed tool contract even when
    /// a background refresh publishes concurrently.
    pub async fn prepare_invocation_from_catalog(
        &self,
        catalog: &ToolCatalogSnapshot,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PreparedActionInvocation> {
        self.require_owned_catalog(catalog)?;
        let registry = &catalog.registry;
        let registered_tool = registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| registry.unknown_tool_error(&invocation.name))?;
        let tool_definition = &registered_tool.definition;
        validate_tool_invocation(tool_definition, invocation)?;
        let capability = registered_tool.execution.capability_id(&invocation.name);
        let policy_input = self.describe_invocation(tool_definition, invocation)?;
        let rules = if let Some(rule_store) = self.bindings.rule_store() {
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
        let mut policy = self.bindings.policies.check(
            &policy_input,
            &capability,
            &moa_security::ActionPolicyContext::from_session(session)
                .with_origin(self.call_origin()),
            &rules,
        )?;
        if let Some(minimum_effect) = registered_tool
            .execution
            .installed_connector_minimum_effect()
        {
            let floored = moa_security::stricter_effect(policy.effect, minimum_effect);
            if floored != policy.effect {
                policy.effect = floored;
                policy.source = ActionPolicyDecisionSource::ToolDefinition;
                policy.reason = Some(
                    "the installed connector binding requires a stricter action-policy effect"
                        .to_string(),
                );
            }
        }
        let needs_review_preview = matches!(policy.effect, ActionPolicyEffect::AdminReview);
        let review_root = if needs_review_preview {
            self.workspace_root(&session.tenant_id)
                .await
                .or_else(|| self.hands.sandbox_root())
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

    fn describe_invocation(
        &self,
        definition: &moa_core::types::tools::ToolDefinition,
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

/// Validates one invocation against its registered Draft 2020-12 input schema.
pub(super) fn validate_tool_invocation(
    definition: &moa_core::types::tools::ToolDefinition,
    invocation: &ToolInvocation,
) -> Result<()> {
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(RejectExternalSchemaRetriever)
        .build(&definition.schema)
        .map_err(|error| {
            MoaError::ValidationError(format!(
                "tool {} has an invalid input schema: {error}",
                definition.name
            ))
        })?;
    if let Some(error) = validator.iter_errors(&invocation.input).next() {
        let instance_path = error.instance_path().to_string();
        let instance_path = if instance_path.is_empty() {
            "/"
        } else {
            instance_path.as_str()
        };
        return Err(MoaError::ValidationError(format!(
            "tool {} input at {instance_path}: {error}",
            definition.name
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct RejectExternalSchemaRetriever;

impl Retrieve for RejectExternalSchemaRetriever {
    fn retrieve(
        &self,
        uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn StdError + Send + Sync>> {
        Err(format!("external tool schema retrieval is disabled: {uri}").into())
    }
}

fn identity_actor_for_policy_lookup(session: &SessionMeta) -> UserId {
    match &session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId(id.to_string()),
        _ => UserId(Uuid::nil().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        error::MoaError,
        types::action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        types::completion::ToolInvocation,
        types::identifiers::TenantId,
        types::session::SessionMeta,
        types::tools::{IdempotencyClass, ToolDiffStrategy, ToolInputShape, ToolPolicySpec},
    };
    use serde_json::json;
    use std::collections::HashMap;
    use uuid::Uuid;

    use super::ToolRouter;
    use crate::core::registration::ToolRegistry;

    fn session() -> SessionMeta {
        SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(7)),
            ..SessionMeta::default()
        }
    }

    fn admin_review_json_policy() -> ToolPolicySpec {
        ToolPolicySpec {
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::AdminReview,
            action_class: ActionClass::ExternalWrite,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        }
    }

    #[tokio::test]
    async fn provider_execution_lifecycle_names_are_not_synthetic_policy_tools() {
        // Pins: execution lifecycle is entered through typed orchestration, so a model-facing
        // lifecycle name absent from the registry remains unknown and fails closed.
        let router = ToolRouter::new(
            ToolRegistry::new(),
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        let session = session();

        let unknown = router
            .check_policy(
                &session,
                &ToolInvocation {
                    id: None,
                    name: "execution_run_start".to_string(),
                    input: json!({}),
                },
            )
            .await;
        let error = unknown.expect_err("unregistered lifecycle tools must fail closed");
        assert_eq!(
            error.to_string(),
            "tool error: unknown tool: execution_run_start"
        );
    }

    #[tokio::test]
    async fn check_policy_rejects_invalid_registered_tool_input_before_review() {
        // Pins: every registered definition's schema is enforced before policy review, not only
        // at a concrete hand or MCP executor.
        let mut registry = ToolRegistry::new();
        registry.register_hand(
            "lookup_filing",
            "Lookup a filing",
            json!({
                "type": "object",
                "properties": {"item_key": {"type": "string"}},
                "required": ["item_key"],
                "additionalProperties": false
            }),
            admin_review_json_policy(),
            IdempotencyClass::NonIdempotent,
        );
        let router = ToolRouter::new(
            registry,
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );

        let error = router
            .check_policy(
                &session(),
                &ToolInvocation {
                    id: None,
                    name: "lookup_filing".to_string(),
                    input: json!({"item_key": 7}),
                },
            )
            .await
            .expect_err("invalid input must not reach policy review");

        match error {
            MoaError::ValidationError(message) => {
                assert!(message.contains("lookup_filing"));
                assert!(message.contains("/item_key"));
                assert!(message.contains("string"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn registered_tool_schema_cannot_retrieve_external_references() {
        // Pins: provider-controlled schemas cannot turn validation into an outbound network or
        // file retrieval before policy review.
        let mut registry = ToolRegistry::new();
        registry.register_hand(
            "external_schema",
            "Tool with an untrusted remote schema reference",
            json!({"$ref": "https://schemas.example.test/tool-input.json"}),
            admin_review_json_policy(),
            IdempotencyClass::NonIdempotent,
        );
        let router = ToolRouter::new(
            registry,
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );

        let error = router
            .check_policy(
                &session(),
                &ToolInvocation {
                    id: None,
                    name: "external_schema".to_string(),
                    input: json!({}),
                },
            )
            .await
            .expect_err("external schema retrieval must fail closed");

        match error {
            MoaError::ValidationError(message) => {
                assert!(message.contains("external_schema"));
                assert!(message.contains("external tool schema retrieval is disabled"));
            }
            other => panic!("expected ValidationError, got {other:?}"),
        }
    }
}
