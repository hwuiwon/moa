//! Restate virtual object that owns one durable conversational sub-agent.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::Utc;
use moa_core::{
    ActiveSegment, ApprovalDecision, CompletionRequest, CompletionResponse, ContextMessage,
    DispatchSubAgentInput, Event, MoaError, ModelCapabilities, ModelId, SessionId, SessionMeta,
    SessionStatus, SubAgentChildRef, SubAgentId, SubAgentMessage, SubAgentResult, SubAgentState,
    SubAgentStatus, ToolCallId, ToolInvocation, ToolOutput, TurnOutcome, UserId, UserMessage,
    WorkspaceId, dispatch_sub_agent_tool_schema,
};
use restate_sdk::prelude::*;
use serde_json::json;

use crate::OrchestratorCtx;
use crate::observability::annotate_restate_handler_span;
use crate::services::session_store::{
    AppendEventRequest, RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest,
    RestateSessionStoreClient,
};
use crate::sub_agent_dispatch::{DispatchedSubAgent, MAX_SUB_AGENT_DEPTH, dispatch_sub_agent};
use crate::turn::approval::serialize_awakeable_decision;
use crate::turn::util::{
    apply_response_to_history, dispatch_history_text, summarize_response_text,
};
use crate::turn::{AgentAdapter, TurnRunner};
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec};

mod adapter;
mod handlers;
mod persistence;
mod request;
mod state;

use adapter::SubAgentTurnAdapter;
use persistence::{persist_parent_session_event, render_user_message, to_handler_error};
use request::{build_completion_request, synthetic_session_meta};
pub use state::SubAgentVoState;
use state::{K_BUDGET_REMAINING, K_CHILDREN, K_PENDING_APPROVAL, MAX_TURNS_PER_POST};

/// Restate virtual object surface for one conversational sub-agent.
#[restate_sdk::object]
pub trait SubAgent {
    /// Parent dispatches a message (initial task or follow-up).
    async fn post_message(msg: Json<SubAgentMessage>) -> Result<(), HandlerError>;

    /// Returns read-only child status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SubAgentStatus>, HandlerError>;

    /// Requests cooperative cancellation for the child.
    async fn cancel(reason: String) -> Result<(), HandlerError>;

    /// Resolves the currently pending approval decision for the child.
    #[shared]
    async fn approve(decision: Json<ApprovalDecision>) -> Result<(), HandlerError>;

    /// Runs one conversational turn for the child.
    async fn run_turn() -> Result<Json<TurnOutcome>, HandlerError>;

    /// Clears all persisted state for this child key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `SubAgent` virtual object implementation.
pub struct SubAgentImpl;
