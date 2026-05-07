//! Restate virtual object that owns one durable MOA session key.

use std::time::Instant;

use chrono::Utc;
use moa_brain::intents::IntentClassifier;
use moa_brain::pipeline::segments::SegmentTracker;
use moa_brain::resolution::{
    ResolutionOverride, ResolutionScorer, continuation_signal, self_assessment_signal,
    structural_signal, tool_signal, verification_signal,
};
use moa_core::{
    ActiveSegment, ApprovalDecision, CancelMode, CompletionRequest, CompletionResponse,
    DispatchSubAgentInput, Event, EventRange, EventRecord, LearningEntry, MessageRole, MoaError,
    QueryRewriteResult, Result as MoaResult, ScoringPhase, SegmentId, SessionId, SessionMeta,
    SessionStatus, SubAgentChildRef, SubAgentId, ToolCallId, ToolInvocation, ToolOutput,
    TurnOutcome, UserMessage, record_session_error, record_turn_event_persist_duration,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::brain_bridge::{PreparedTurnRequest, prepare_turn_request};
use crate::ctx::OrchestratorCtx;
use crate::objects::sub_agent::SubAgentClient;
use crate::observability::{annotate_restate_handler_span, event_persist_span};
use crate::services::session_store::{
    AppendEventRequest, RestateSessionStoreClient, UpdateStatusRequest,
};
use crate::services::session_store::{
    CompleteSegmentRequest, CreateSegmentRequest, GetEventsRequest, GetSegmentBaselineRequest,
    RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest,
    UpdateSegmentResolutionScoreRequest,
};
use crate::sub_agent_dispatch::{DispatchedSubAgent, dispatch_sub_agent};
use crate::turn::approval::serialize_awakeable_decision;
use crate::turn::util::summarize_response_text;
use crate::turn::{AgentAdapter, TurnRunner};
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_vec};

mod adapter;
mod handlers;
mod persistence;
mod scoring;
mod segments;
mod state;

use adapter::SessionTurnAdapter;
use persistence::{parse_session_key, persist_session_event, sync_status, to_handler_error};
use scoring::{score_active_segment, score_completed_segment_at_transition};
use segments::ensure_current_segment;
pub use state::SessionVoState;
use state::{K_CHILDREN, K_PENDING_APPROVAL, MAX_TURNS_PER_POST};

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

    /// Runs one brain turn against the durable event log and Restate services.
    async fn run_turn() -> Result<Json<TurnOutcome>, HandlerError>;

    /// Clears all persisted VO state for this session key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `Session` virtual object implementation.
pub struct SessionImpl;
