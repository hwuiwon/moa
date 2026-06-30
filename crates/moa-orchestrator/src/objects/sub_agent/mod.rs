//! Restate virtual object that owns one durable conversational sub-agent.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::wire::session_store::{AppendEventRequest, RecordSegmentTurnUsageRequest};
use moa_core::{
    AgentSignalId, AttachSubAgentResultWaiterInput, AttachSubAgentResultWaiterOutput,
    ChildSignalKind, CompleteSubAgentChildInput, CompletionRequest,
    ConsumeSubAgentChildResultInput, ConsumeSubAgentChildResultOutput, ContextMessage, Event,
    MarkSubAgentChildTerminalInput, MoaError, ModelCapabilities, ModelId, ParentResumePolicy,
    RemoveSubAgentResultWaiterInput, ReserveSubAgentInput, ReservedSubAgent, SessionId,
    SessionMeta, SessionStatus, SignalSeverity, SubAgentChildRef, SubAgentId, SubAgentMessage,
    SubAgentProgressSummary, SubAgentResult, SubAgentSignal, SubAgentState, SubAgentStatus,
    SubAgentTerminalResult, SubAgentToolRecord, SubAgentTurnOutcomeRecord, SubAgentTurnPreparation,
    SubAgentTurnResponseRecord, TenantId, TrustedSandboxFileManifestRef, TurnOutcome, UserId,
    UserMessage, delegation_tool_schemas,
};
use restate_sdk::prelude::*;
use serde_json::json;

use crate::OrchestratorCtx;
use crate::objects::durable_utc_now;
use crate::services::session_store::RestateSessionStoreClient;
use crate::sub_agent_dispatch::{
    MAX_SUB_AGENT_DEPTH, child_agent_path, refund_child_budget, reserve_child_budget,
    validate_dispatch_budget, validate_dispatch_limits,
};
use crate::turn::util::{apply_response_to_history, summarize_response_text};
use crate::vo::{
    VoReader, VoState, schedule_generation_guarded_self_call, set_or_clear_opt,
    set_or_clear_scalar, set_or_clear_vec,
};
use moa_observability::restate_observability::annotate_restate_handler_span;

mod handlers;
mod persistence;
mod request;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use persistence::{persist_parent_session_event, render_user_message};
use request::{build_completion_request, synthetic_session_meta};
use state::MAX_TURNS_PER_POST;
pub use state::SubAgentVoState;

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

    /// Returns a compact fan-in progress summary read on demand by the coordinator.
    #[shared]
    async fn progress_summary() -> Result<Json<SubAgentProgressSummary>, HandlerError>;

    /// Refreshes the telemetry-plane heartbeat timestamp (VO state only, no event).
    async fn record_heartbeat(at: Json<DateTime<Utc>>) -> Result<(), HandlerError>;

    /// Returns the terminal child result when the child has finished.
    #[shared]
    async fn result() -> Result<Json<Option<SubAgentResult>>, HandlerError>;

    /// Requests cooperative cancellation for the child.
    async fn cancel(reason: String) -> Result<(), HandlerError>;

    /// Compiles the next turn input or returns an immediate child outcome.
    async fn prepare_turn() -> Result<Json<SubAgentTurnPreparation>, HandlerError>;

    /// Records one turn-scoped LLM response into child-local history and budget state.
    async fn record_response(
        response: Json<SubAgentTurnResponseRecord>,
    ) -> Result<(), HandlerError>;

    /// Records one executed tool result into child-local history.
    async fn record_tool_result(record: Json<SubAgentToolRecord>) -> Result<(), HandlerError>;

    /// Records one blocked or denied tool result into child-local history.
    async fn record_denied_tool(record: Json<SubAgentToolRecord>) -> Result<(), HandlerError>;

    /// Applies a turn-scoped core turn outcome to child lifecycle state.
    async fn apply_turn_outcome(
        outcome: Json<SubAgentTurnOutcomeRecord>,
    ) -> Result<(), HandlerError>;

    /// Reserves a nested child under this sub-agent.
    async fn reserve_child(
        input: Json<ReserveSubAgentInput>,
    ) -> Result<Json<ReservedSubAgent>, HandlerError>;

    /// Removes a terminal nested child and refunds unused budget.
    async fn complete_child(input: Json<CompleteSubAgentChildInput>) -> Result<(), HandlerError>;

    /// Caches a nested child terminal result until a wait consumes it.
    async fn mark_child_terminal(
        input: Json<MarkSubAgentChildTerminalInput>,
    ) -> Result<(), HandlerError>;

    /// Consumes a cached nested child terminal result.
    async fn consume_child_result(
        input: Json<ConsumeSubAgentChildResultInput>,
    ) -> Result<Json<ConsumeSubAgentChildResultOutput>, HandlerError>;

    /// Registers a workflow awakeable that should resolve when this child terminates.
    async fn attach_result_waiter(
        input: Json<AttachSubAgentResultWaiterInput>,
    ) -> Result<Json<AttachSubAgentResultWaiterOutput>, HandlerError>;

    /// Removes a workflow awakeable after a bounded wait times out.
    async fn remove_result_waiter(
        input: Json<RemoveSubAgentResultWaiterInput>,
    ) -> Result<(), HandlerError>;

    /// Lists active nested children owned by this sub-agent.
    #[shared]
    async fn child_refs() -> Result<Json<Vec<SubAgentChildRef>>, HandlerError>;

    /// Records the terminal outcome delivered by a sub-agent turn workflow.
    async fn record_turn_outcome(
        outcome: Json<moa_core::wire::turn::TurnOutcome>,
    ) -> Result<(), HandlerError>;

    /// Clears all persisted state for this child key.
    async fn destroy() -> Result<(), HandlerError>;

    /// Report-then-self-clean: releases this terminal child's fan-out and VO state.
    ///
    /// Scheduled as a generation-guarded delayed self-call after terminal delivery. It
    /// no-ops when revived/superseded, defers while the child still has non-terminal
    /// children (bottom-up teardown), and otherwise removes the child from the parent
    /// fan-out and clears its VO state once the report is durable.
    async fn cleanup(req: Json<CleanupRequest>) -> Result<(), HandlerError>;
}

/// Internal payload for a generation-guarded report-then-self-clean self-call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CleanupRequest {
    /// Cleanup generation observed when this tick was scheduled. A tick whose
    /// generation no longer matches the VO's current `cleanup_generation` is stale —
    /// the child was revived or the cleanup was rescheduled — and is ignored.
    pub generation: u64,
}

/// Concrete `SubAgent` virtual object implementation.
pub struct SubAgentImpl;
