//! Policy evaluation and approval rendering for tool invocations.

use moa_core::{
    ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, ApprovalRule, MoaError,
    PolicyAction, Result, SessionMeta, ToolInvocation, ToolPolicyInput, UserId,
};
use uuid::Uuid;

use super::ToolRouter;
use super::normalization::{
    approval_diffs_for, approval_fields_for, approval_pattern_for, normalized_input_for,
    summary_for,
};

/// Prepared metadata for a concrete tool invocation.
#[derive(Debug, Clone)]
pub struct PreparedToolInvocation {
    /// Normalized policy-facing description of the invocation.
    policy_input: ToolPolicyInput,
    /// Result of evaluating the invocation against the active policies.
    policy: moa_security::PolicyCheck,
    /// Suggested rule pattern for "Always Allow".
    always_allow_pattern: String,
    /// Structured approval fields for the local UI.
    approval_fields: Vec<ApprovalField>,
    /// Optional inline file diffs for the local UI.
    approval_diffs: Vec<ApprovalFileDiff>,
}

impl PreparedToolInvocation {
    /// Returns the policy evaluation outcome for the invocation.
    pub fn policy(&self) -> &moa_security::PolicyCheck {
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

    /// Builds the approval prompt for this invocation with the given request identifier.
    pub fn approval_prompt(&self, request_id: Uuid) -> ApprovalPrompt {
        ApprovalPrompt {
            request: ApprovalRequest {
                request_id,
                tool_name: self.policy_input.tool_name.clone(),
                input_summary: self.policy_input.input_summary.clone(),
                risk_level: self.policy_input.risk_level.clone(),
            },
            pattern: self.always_allow_pattern.clone(),
            parameters: self.approval_fields.clone(),
            file_diffs: self.approval_diffs.clone(),
        }
    }
}

impl ToolRouter {
    /// Evaluates the policy action for a tool invocation in the current session.
    pub async fn check_policy(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<moa_security::PolicyCheck> {
        Ok(self
            .prepare_invocation(session, invocation)
            .await?
            .policy()
            .clone())
    }

    /// Prepares a tool invocation for policy evaluation and approval rendering.
    pub async fn prepare_invocation(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<PreparedToolInvocation> {
        let tool_definition = self
            .registry
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;
        let policy_input = self.describe_invocation(tool_definition, invocation)?;
        let rules = if let Some(rule_store) = &self.rule_store {
            rule_store
                .list_approval_rules(&session.workspace_id)
                .await?
        } else {
            Vec::new()
        };
        let policy = self.policies.check(
            &policy_input,
            &moa_security::ToolPolicyContext::from_session(session),
            &rules,
        )?;
        let approval_root = self
            .workspace_roots
            .read()
            .await
            .get(&session.workspace_id)
            .cloned()
            .or_else(|| self.sandbox_root.clone());

        Ok(PreparedToolInvocation {
            always_allow_pattern: approval_pattern_for(
                tool_definition.policy.input_shape,
                &policy_input.normalized_input,
            ),
            approval_fields: approval_fields_for(
                approval_root.as_deref(),
                tool_definition.policy.input_shape,
                invocation,
            ),
            approval_diffs: approval_diffs_for(
                approval_root.as_deref(),
                tool_definition.policy.diff_strategy,
                invocation,
            )
            .await?,
            policy_input,
            policy,
        })
    }

    /// Persists an approval rule for the current workspace.
    pub async fn store_approval_rule(
        &self,
        session: &SessionMeta,
        tool: &str,
        pattern: &str,
        action: PolicyAction,
        created_by: UserId,
    ) -> Result<()> {
        let Some(rule_store) = &self.rule_store else {
            return Err(MoaError::Unsupported(
                "tool router does not have an approval rule store".to_string(),
            ));
        };

        rule_store
            .upsert_approval_rule(ApprovalRule {
                id: Uuid::now_v7(),
                workspace_id: session.workspace_id.clone(),
                tool: tool.to_string(),
                pattern: pattern.to_string(),
                action,
                scope: moa_core::PolicyScope::Workspace,
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
            risk_level: definition.policy.risk_level.clone(),
            default_action: definition.policy.default_action.clone(),
        })
    }
}
