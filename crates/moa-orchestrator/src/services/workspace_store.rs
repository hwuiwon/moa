//! Restate service for workspace-scoped action-policy checks.

use std::sync::Arc;

use moa_core::{
    ActionEnvelope, ActionPolicyEffect, ActionPolicyRule, ActionReviewPreview, MoaError,
    SessionMeta, SubAgentId, ToolCallId, ToolInvocation,
};
use moa_hands::{ActionOrigin, ToolRouter};
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
                let origin = ActionOrigin {
                    origin_kind: request.origin_kind,
                    origin_id: request.origin_id,
                    origin_step_id: request.origin_step_id,
                    idempotency_key: request.idempotency_key,
                };
                Ok(Json::from(PreparedActionReview {
                    effect: prepared.policy().effect,
                    reason: prepared.policy().reason.clone(),
                    matched_rule: prepared.policy().matched_rule.clone(),
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

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}
