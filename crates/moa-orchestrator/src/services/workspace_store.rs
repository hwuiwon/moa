//! Restate service for workspace-scoped tool policy checks and approval rule writes.

use std::sync::Arc;

use moa_core::{
    ApprovalPrompt, ApprovalRule, MoaError, PolicyAction, SessionMeta, ToolInvocation, UserId,
};
use moa_hands::ToolRouter;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::observability::annotate_restate_handler_span;

/// Request payload for `WorkspaceStore/prepare_tool_approval`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PrepareToolApprovalRequest {
    /// Session metadata used for workspace-scoped policy evaluation.
    pub session: SessionMeta,
    /// Tool invocation that is about to execute.
    pub invocation: ToolInvocation,
    /// Stable approval request identifier to embed into the rendered prompt.
    pub request_id: Uuid,
}

/// Prepared policy decision and optional approval prompt for one tool call.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreparedToolApproval {
    /// Final policy action for this invocation.
    pub action: PolicyAction,
    /// Matching approval rule when the decision came from persisted policy.
    pub matched_rule: Option<ApprovalRule>,
    /// Human-readable invocation summary.
    pub input_summary: String,
    /// Approval prompt rendered for the UI when approval is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<ApprovalPrompt>,
}

/// Request payload for `WorkspaceStore/store_approval_rule`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoreApprovalRuleRequest {
    /// Session metadata used to determine the owning workspace.
    pub session: SessionMeta,
    /// Tool name the rule applies to.
    pub tool_name: String,
    /// Glob pattern or normalized shell pattern to persist.
    pub pattern: String,
    /// Rule action to store.
    pub action: PolicyAction,
    /// User that approved the rule.
    pub created_by: UserId,
}

/// Restate service surface for workspace-scoped approval policy operations.
#[restate_sdk::service]
pub trait WorkspaceStore {
    /// Evaluates policy for one tool invocation and prepares an approval prompt when needed.
    async fn prepare_tool_approval(
        request: Json<PrepareToolApprovalRequest>,
    ) -> Result<Json<PreparedToolApproval>, HandlerError>;

    /// Persists a workspace-scoped approval rule.
    async fn store_approval_rule(
        request: Json<StoreApprovalRuleRequest>,
    ) -> Result<(), HandlerError>;
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
    async fn prepare_tool_approval(
        &self,
        ctx: Context<'_>,
        request: Json<PrepareToolApprovalRequest>,
    ) -> Result<Json<PreparedToolApproval>, HandlerError> {
        annotate_restate_handler_span("WorkspaceStore", "prepare_tool_approval");
        let request = request.into_inner();
        let router = self.router.clone();

        Ok(ctx
            .run(|| async move {
                let prepared = router
                    .prepare_invocation(&request.session, &request.invocation)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json::from(PreparedToolApproval {
                    action: prepared.policy().action.clone(),
                    matched_rule: prepared.policy().matched_rule.clone(),
                    input_summary: prepared.input_summary().to_string(),
                    prompt: matches!(prepared.policy().action, PolicyAction::RequireApproval)
                        .then(|| prepared.approval_prompt(request.request_id)),
                }))
            })
            .name("prepare_tool_approval")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn store_approval_rule(
        &self,
        ctx: Context<'_>,
        request: Json<StoreApprovalRuleRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("WorkspaceStore", "store_approval_rule");
        let request = request.into_inner();
        let router = self.router.clone();

        Ok(ctx
            .run(|| async move {
                router
                    .store_approval_rule(
                        &request.session,
                        &request.tool_name,
                        &request.pattern,
                        request.action,
                        request.created_by,
                    )
                    .await
                    .map_err(to_handler_error)
            })
            .name("store_approval_rule")
            .await?)
    }
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}
