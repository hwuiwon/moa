//! Restate virtual object that owns one durable conversational sub-agent.

use std::collections::HashMap;

use chrono::Utc;
use moa_core::{
    ApprovalDecision, CompleteSubAgentChildInput, CompletionRequest, CompletionResponse,
    ContextMessage, Event, MoaError, ModelCapabilities, ModelId, ReserveSubAgentInput,
    ReservedSubAgent, SessionId, SessionMeta, SessionStatus, SubAgentChildRef, SubAgentId,
    SubAgentMessage, SubAgentResult, SubAgentState, SubAgentStatus, SubAgentToolRecord,
    SubAgentTurnPreparation, TurnOutcome, UserId, UserMessage, WorkspaceId,
    delegation_tool_schemas, dispatch_sub_agent_tool_schema,
};
use restate_sdk::prelude::*;
use serde_json::json;

use crate::OrchestratorCtx;
use crate::services::session_store::{
    AppendEventRequest, RecordSegmentTurnUsageRequest, RestateSessionStoreClient,
};
use crate::sub_agent_dispatch::{
    MAX_SUB_AGENT_DEPTH, child_agent_path, refund_child_budget, remove_child_ref,
    reserve_child_budget, validate_dispatch_budget, validate_dispatch_limits,
};
use crate::turn::approval::serialize_awakeable_decision;
use crate::turn::util::{apply_response_to_history, summarize_response_text};
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec};
use moa_core::restate_observability::annotate_restate_handler_span;

mod handlers;
mod persistence;
mod request;
mod state;

use persistence::{persist_parent_session_event, render_user_message, to_handler_error};
use request::{build_completion_request, synthetic_session_meta};
pub use state::SubAgentVoState;
use state::{K_PENDING_APPROVAL, MAX_TURNS_PER_POST};

/// Maximum one-turn runner iterations a sub-agent workflow may execute before failing.
pub(crate) const MAX_SUB_AGENT_TURNS_PER_WORKFLOW: usize = MAX_TURNS_PER_POST;

/// Restate virtual object surface for one conversational sub-agent.
#[restate_sdk::object]
pub trait SubAgent {
    /// Parent dispatches a message (initial task or follow-up).
    async fn post_message(msg: Json<SubAgentMessage>) -> Result<(), HandlerError>;

    /// Returns read-only child status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SubAgentStatus>, HandlerError>;

    /// Returns the terminal child result when the child has finished.
    #[shared]
    async fn result() -> Result<Json<Option<SubAgentResult>>, HandlerError>;

    /// Requests cooperative cancellation for the child.
    async fn cancel(reason: String) -> Result<(), HandlerError>;

    /// Resolves the currently pending approval decision for the child.
    #[shared]
    async fn approve(decision: Json<ApprovalDecision>) -> Result<(), HandlerError>;

    /// Compiles the next turn input or returns an immediate child outcome.
    async fn prepare_turn() -> Result<Json<SubAgentTurnPreparation>, HandlerError>;

    /// Records one LLM response into child-local history and budget state.
    async fn record_response(response: Json<CompletionResponse>) -> Result<(), HandlerError>;

    /// Records one executed tool result into child-local history.
    async fn record_tool_result(record: Json<SubAgentToolRecord>) -> Result<(), HandlerError>;

    /// Records one blocked or denied tool result into child-local history.
    async fn record_denied_tool(record: Json<SubAgentToolRecord>) -> Result<(), HandlerError>;

    /// Applies the core turn outcome to child lifecycle state.
    async fn apply_turn_outcome(outcome: Json<TurnOutcome>) -> Result<(), HandlerError>;

    /// Marks the child as waiting on an approval awakeable.
    async fn set_pending_approval(awakeable_id: Json<String>) -> Result<(), HandlerError>;

    /// Clears the child approval marker after the workflow resumes.
    async fn clear_pending_approval() -> Result<(), HandlerError>;

    /// Reserves a nested child under this sub-agent.
    async fn reserve_child(
        input: Json<ReserveSubAgentInput>,
    ) -> Result<Json<ReservedSubAgent>, HandlerError>;

    /// Removes a terminal nested child and refunds unused budget.
    async fn complete_child(input: Json<CompleteSubAgentChildInput>) -> Result<(), HandlerError>;

    /// Lists active nested children owned by this sub-agent.
    #[shared]
    async fn child_refs() -> Result<Json<Vec<SubAgentChildRef>>, HandlerError>;

    /// Records the terminal outcome delivered by a sub-agent turn workflow.
    async fn record_turn_outcome(
        outcome: Json<moa_core::wire::TurnOutcome>,
    ) -> Result<(), HandlerError>;

    /// Clears all persisted state for this child key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `SubAgent` virtual object implementation.
pub struct SubAgentImpl;
