//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_core::wire::{
    CancelResponse, PendingMessage, QueueMessageRequest, QueueMessageResponse, RunTurnRequest,
    SessionSnapshot, StartTurnRequest, StartTurnResponse, TurnOutcome as ExecutionTurnOutcome,
    TurnOutcomeKind as ExecutionTurnOutcomeKind,
};
use moa_core::{
    ActiveSegment, ApprovalDecision, CancelMode, Event, EventRange, MoaError, Result as MoaResult,
    SessionId, SessionMeta, SessionStatus, SubAgentChildRef, TurnOutcome, UserMessage,
    record_turn_event_persist_duration,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::objects::sub_agent::SubAgentClient;
use crate::services::session_store::{RestateSessionStoreClient, UpdateStatusRequest};
use crate::turn::approval::serialize_awakeable_decision;
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_vec};
use crate::workflows::turn_execution::TurnExecutionClient;
use moa_core::restate_observability::{annotate_restate_handler_span, event_persist_span};

mod handlers;
mod persistence;
mod state;

use persistence::{parse_session_key, sync_status, to_handler_error};
use state::K_PENDING_APPROVAL;
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

    /// Resolves the currently pending approval decision for the blocked turn.
    #[shared]
    async fn approve(decision: Json<ApprovalDecision>) -> Result<(), HandlerError>;

    /// Requests a cooperative soft or hard cancellation.
    async fn cancel(mode: Json<CancelMode>) -> Result<(), HandlerError>;

    /// Returns the current durable lifecycle status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SessionStatus>, HandlerError>;

    /// Legacy internal turn runner entrypoint; retained only for wire compatibility.
    async fn run_turn() -> Result<Json<TurnOutcome>, HandlerError>;

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

fn dispatch_turn_execution(
    ctx: &ObjectContext<'_>,
    turn_id: String,
    user_message: String,
    attachments: Vec<moa_core::Attachment>,
    model: Option<String>,
) {
    ctx.workflow_client::<TurnExecutionClient>(turn_id.clone())
        .run(Json::from(RunTurnRequest {
            session_id: ctx.key().to_string(),
            turn_id,
            user_message,
            attachments,
            model,
        }))
        .send();
}
