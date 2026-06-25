//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_core::wire::{
    CancelResponse, PendingMessage, QueueMessageRequest, QueueMessageResponse, RunTurnRequest,
    SessionProgress, SessionProgressRequest, SessionSnapshot, StartTurnRequest, StartTurnResponse,
    TurnOutcome as ExecutionTurnOutcome, TurnOutcomeKind as ExecutionTurnOutcomeKind, TurnProgress,
    UpdateStatusRequest,
};
use moa_core::{
    ActiveSegment, CancelMode, ConsumeSubAgentChildResultInput, ConsumeSubAgentChildResultOutput,
    ContactRef, EventRange, EventRecord, MarkSubAgentChildTerminalInput, MoaError,
    Result as MoaResult, SessionId, SessionMeta, SessionStatus, SessionStore as _,
    SubAgentChildRef, SubAgentTerminalResult, UserMessage,
};
use moa_observability::record_turn_event_persist_duration;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::objects::sub_agent::SubAgentClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::RestateSessionStoreClient;
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_vec};
use crate::workflows::turn_execution::TurnExecutionClient;
use moa_observability::restate_observability::{annotate_restate_handler_span, event_persist_span};

mod handlers;
mod persistence;
mod state;

use persistence::{parse_session_key, sync_status, to_handler_error};
pub use state::SessionVoState;

const K_PENDING_STATE: &str = "pending_state";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SessionPendingState {
    active_turn_id: Option<String>,
    pending_messages: VecDeque<PendingMessage>,
    last_outcome: Option<ExecutionTurnOutcome>,
}

/// Restate virtual object surface for one durable session key.
#[restate_sdk::object]
pub trait Session {
    /// Initializes VO state after `SessionStore/create_session` persists metadata in Postgres.
    async fn set_meta(meta: Json<SessionMeta>) -> Result<(), HandlerError>;

    /// Appends a user message and drives turns until the session becomes idle or blocked.
    async fn post_message(msg: Json<UserMessage>) -> Result<(), HandlerError>;

    /// Requests a cooperative soft or hard cancellation.
    async fn cancel(mode: Json<CancelMode>) -> Result<(), HandlerError>;

    /// Returns the current durable lifecycle status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SessionStatus>, HandlerError>;

    /// Starts a new turn through the additive `TurnExecution` workflow path.
    async fn start_turn(
        req: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError>;

    /// Records the terminal outcome delivered by a `TurnExecution` workflow.
    async fn record_turn_outcome(outcome: Json<ExecutionTurnOutcome>) -> Result<(), HandlerError>;

    /// Forwards a cancellation request to the active `TurnExecution` workflow.
    async fn request_cancel(reason: Json<String>) -> Result<Json<CancelResponse>, HandlerError>;

    /// Queues a user message or starts a turn immediately when no turn is active.
    async fn queue_message(
        req: Json<QueueMessageRequest>,
    ) -> Result<Json<QueueMessageResponse>, HandlerError>;

    /// Returns a read-only snapshot of the additive `TurnExecution` lifecycle state.
    #[shared]
    async fn snapshot() -> Result<Json<SessionSnapshot>, HandlerError>;

    /// Returns session snapshot, active-turn progress, and recent durable events in one call.
    #[shared]
    async fn progress(
        req: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError>;

    /// Registers a root-owned child sub-agent for later turns and cancellation.
    async fn register_child(child: Json<SubAgentChildRef>) -> Result<(), HandlerError>;

    /// Removes a root-owned child sub-agent from the active registry.
    async fn remove_child(sub_agent_id: String) -> Result<(), HandlerError>;

    /// Caches a root child terminal result until a wait consumes it.
    async fn mark_child_terminal(
        input: Json<MarkSubAgentChildTerminalInput>,
    ) -> Result<(), HandlerError>;

    /// Consumes a cached root child terminal result.
    async fn consume_child_result(
        input: Json<ConsumeSubAgentChildResultInput>,
    ) -> Result<Json<ConsumeSubAgentChildResultOutput>, HandlerError>;

    /// Lists root-owned active child sub-agents.
    #[shared]
    async fn child_refs() -> Result<Json<Vec<SubAgentChildRef>>, HandlerError>;

    /// Clears all persisted VO state for this session key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `Session` virtual object implementation.
pub struct SessionImpl;

async fn load_pending_state<R: VoReader>(reader: &R) -> Result<SessionPendingState, HandlerError> {
    Ok(reader.get_json(K_PENDING_STATE).await?.unwrap_or_default())
}

fn persist_pending_state(ctx: &ObjectContext<'_>, state: &SessionPendingState) {
    ctx.set(K_PENDING_STATE, Json::from(state.clone()));
}

async fn durable_utc_now(ctx: &ObjectContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .await?
        .into_inner())
}

fn generate_turn_id(ctx: &mut ObjectContext<'_>) -> String {
    ctx.rand_uuid().to_string()
}

fn dispatch_turn_execution(ctx: &ObjectContext<'_>, request: RunTurnRequest) {
    let turn_id = request.turn_id.clone();
    let identity = request.identity.clone();
    let request = ctx
        .workflow_client::<TurnExecutionClient>(turn_id.clone())
        .run(Json::from(request));
    with_identity_headers(request, &identity).send();
}
