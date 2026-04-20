//! Restate virtual object that owns one durable MOA session key.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use moa_core::{
    ApprovalDecision, CancelMode, CompletionContent, CompletionResponse, Event, MoaError,
    PolicyAction, Result as MoaResult, SessionId, SessionMeta, SessionStatus, StopReason,
    SubAgentChildRef, ToolCallContent, ToolCallId, ToolCallRequest, ToolContent, ToolInvocation,
    ToolOutput, TraceContext, TurnLatencyCounters, TurnOutcome, TurnReplayCounters, UserMessage,
    current_turn_root_span, dispatch_sub_agent_tool_schema, record_approval_wait,
    record_session_error, record_turn_event_persist_duration, record_turn_latency,
    record_turn_llm_call_duration, record_turn_tool_dispatch_duration, scope_turn_latency_counters,
    scope_turn_replay_counters,
};
use restate_sdk::prelude::*;
use tracing::Instrument;
use uuid::Uuid;

use crate::brain_bridge::{PreparedTurnRequest, prepare_turn_request};
use crate::objects::sub_agent::SubAgentClient;
use crate::observability::{
    add_session_trace_link, annotate_restate_handler_span, apply_session_trace,
    emit_turn_latency_summary, emit_turn_replay_summary,
};
use crate::runtime::CONFIG;
use crate::services::llm_gateway::LLMGatewayClient;
use crate::services::session_store::{AppendEventRequest, SessionStoreClient, UpdateStatusRequest};
use crate::services::tool_executor::ToolExecutorClient;
use crate::services::workspace_store::{
    PrepareToolApprovalRequest, StoreApprovalRuleRequest, WorkspaceStoreClient,
};
use crate::sub_agent_dispatch::{dispatch_sub_agent, sub_agent_result_tool_output};

const K_META: &str = "meta";
const K_STATUS: &str = "status";
const K_PENDING: &str = "pending";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_CHILDREN: &str = "children";
const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
const K_CANCEL_FLAG: &str = "cancel_flag";
const MAX_TURNS_PER_POST: usize = 50;

/// Serializable projection of the Session VO's durable state keys.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionVoState {
    /// Persisted session metadata mirror.
    pub meta: Option<SessionMeta>,
    /// Current lifecycle status held in Restate state.
    pub status: Option<SessionStatus>,
    /// Buffered user messages waiting to be consumed by `run_turn`.
    pub pending: Vec<UserMessage>,
    /// Placeholder for approval state introduced in R07.
    pub pending_approval: Option<String>,
    /// Placeholder for sub-agent children introduced in R08.
    pub children: Vec<SubAgentChildRef>,
    /// Human-readable stub summary of the last drained turn.
    pub last_turn_summary: Option<String>,
    /// Cooperative cancellation flag checked at turn boundaries.
    pub cancel_flag: Option<CancelMode>,
}

impl SessionVoState {
    /// Initializes the projection from persisted session metadata.
    pub fn set_meta(&mut self, meta: SessionMeta) {
        self.status = Some(meta.status.clone());
        self.meta = Some(meta);
    }

    /// Returns the current lifecycle status, defaulting to `Created` when state is empty.
    pub fn current_status(&self) -> SessionStatus {
        self.status.clone().unwrap_or(SessionStatus::Created)
    }

    /// Ensures that session metadata has been initialized before mutations proceed.
    pub fn ensure_initialized(&self) -> MoaResult<&SessionMeta> {
        self.meta.as_ref().ok_or_else(|| {
            MoaError::ValidationError(
                "Session metadata missing. Initialize the VO via SessionStore/init_session_vo first."
                    .to_string(),
            )
        })
    }

    /// Queues one user message and transitions the session into `Running`.
    pub fn enqueue_message(&mut self, msg: UserMessage) -> MoaResult<()> {
        self.ensure_initialized()?;
        self.pending.push(msg);
        self.set_status(SessionStatus::Running);
        Ok(())
    }

    /// Applies a turn outcome to the lifecycle state.
    ///
    /// In the existing MOA status model, an idle turn parks the session in `Paused`.
    pub fn apply_turn_outcome(&mut self, outcome: TurnOutcome) -> SessionStatus {
        let next_status = match outcome {
            TurnOutcome::Continue => SessionStatus::Running,
            TurnOutcome::Idle => SessionStatus::Paused,
            TurnOutcome::WaitingApproval => SessionStatus::WaitingApproval,
            TurnOutcome::Cancelled => SessionStatus::Cancelled,
        };
        self.set_status(next_status.clone());
        next_status
    }

    /// Records a cooperative cancellation request.
    pub fn set_cancel_flag(&mut self, mode: CancelMode) {
        self.cancel_flag = Some(mode);
    }

    /// Consumes the current cancellation flag, if any.
    pub fn take_cancel_flag(&mut self) -> Option<CancelMode> {
        self.cancel_flag.take()
    }

    /// Drains buffered user messages and records a short stub summary.
    pub fn drain_pending_messages(&mut self) -> usize {
        let drained = self.pending.len();
        self.pending.clear();
        self.last_turn_summary = if drained == 0 {
            None
        } else if drained == 1 {
            Some("drained 1 queued message".to_string())
        } else {
            Some(format!("drained {drained} queued messages"))
        };
        drained
    }

    /// Clears the in-memory projection back to an empty VO.
    pub fn destroy(&mut self) {
        *self = Self::default();
    }

    fn set_status(&mut self, status: SessionStatus) {
        self.status = Some(status.clone());
        if let Some(meta) = self.meta.as_mut() {
            meta.status = status.clone();
            meta.updated_at = Utc::now();
            if matches!(
                status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            ) && meta.completed_at.is_none()
            {
                meta.completed_at = Some(Utc::now());
            }
        }
    }
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

    /// Runs one brain turn against the durable event log and Restate services.
    async fn run_turn() -> Result<Json<TurnOutcome>, HandlerError>;

    /// Clears all persisted VO state for this session key.
    async fn destroy() -> Result<(), HandlerError>;
}

/// Concrete `Session` virtual object implementation.
pub struct SessionImpl;

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    async fn set_meta(
        &self,
        ctx: ObjectContext<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "set_meta");
        let mut state = load_object_state(&ctx).await?;
        state.set_meta(meta.into_inner());
        persist_state(&ctx, &state);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        ctx: ObjectContext<'_>,
        msg: Json<UserMessage>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "post_message");
        let session_id = parse_session_key(ctx.key())?;
        let msg = msg.into_inner();
        let mut state = load_object_state(&ctx).await?;
        let should_start_turn_runner = !matches!(
            state.current_status(),
            SessionStatus::Running | SessionStatus::WaitingApproval
        );
        state
            .enqueue_message(msg.clone())
            .map_err(to_handler_error)?;
        persist_state(&ctx, &state);

        persist_session_event(
            &ctx,
            session_id,
            Event::UserMessage {
                text: msg.text,
                attachments: msg.attachments,
            },
        )
        .await?;
        sync_status(&ctx, session_id, &state).await?;
        if should_start_turn_runner {
            ctx.object_client::<SessionClient>(ctx.key().to_string())
                .run_turn()
                .send();
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, decision))]
    async fn approve(
        &self,
        ctx: SharedObjectContext<'_>,
        decision: Json<ApprovalDecision>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "approve");
        let awakeable_id = ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner)
            .ok_or_else(|| TerminalError::new("no pending approval for this session"))?;
        let decision = decision.into_inner();
        let serialized_decision = serialize_awakeable_decision(&decision)?;

        ctx.resolve_awakeable(&awakeable_id, serialized_decision);
        tracing::info!(
            key = %ctx.key(),
            awakeable_id,
            ?decision,
            "resolved session approval awakeable"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, mode))]
    async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        mode: Json<CancelMode>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "cancel");
        let mut state = load_object_state(&ctx).await?;
        state.set_cancel_flag(mode.into_inner());
        let children = state.children.clone();
        persist_state(&ctx, &state);
        for child in children {
            ctx.object_client::<SubAgentClient>(child.id)
                .cancel("parent session cancelled".to_string())
                .send();
        }
        tracing::info!(mode = ?state.cancel_flag, key = %ctx.key(), "session cancel flag set");
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        annotate_restate_handler_span("Session", "status");
        Ok(Json::from(load_shared_state(&ctx).await?.current_status()))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn run_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("Session", "run_turn");
        let session_id = parse_session_key(ctx.key())?;
        let mut turns_this_invocation = 0usize;
        loop {
            let mut current = load_object_state(&ctx).await?;
            if current.take_cancel_flag().is_some() {
                current.apply_turn_outcome(TurnOutcome::Cancelled);
                persist_state(&ctx, &current);
                sync_status(&ctx, session_id, &current).await?;
                return Ok(Json::from(TurnOutcome::Cancelled));
            }
            persist_state(&ctx, &current);

            turns_this_invocation += 1;
            if turns_this_invocation > MAX_TURNS_PER_POST {
                record_session_error("turn_budget");
                persist_session_event(
                    &ctx,
                    session_id,
                    Event::Error {
                        message: format!("turn budget exceeded ({MAX_TURNS_PER_POST}), stopping"),
                        recoverable: true,
                    },
                )
                .await?;
                let mut current = load_object_state(&ctx).await?;
                current.apply_turn_outcome(TurnOutcome::Idle);
                persist_state(&ctx, &current);
                sync_status(&ctx, session_id, &current).await?;
                return Ok(Json::from(TurnOutcome::Idle));
            }

            let turn_number = turns_this_invocation as i64;
            let turn_prompt = current.pending.last().map(|message| message.text.as_str());
            let turn_root_span = match current.meta.as_ref() {
                Some(meta) => session_turn_span(meta, turn_prompt, turn_number),
                None => {
                    tracing::info_span!("session_turn", otel.name = %format!("MOA turn {turn_number}"))
                }
            };
            let turn_counters = Arc::new(TurnReplayCounters::default());
            let turn_outcome = scope_turn_replay_counters(turn_counters.clone(), async {
                let turn_latency_counters =
                    Arc::new(TurnLatencyCounters::new(turn_root_span.clone()));
                let turn_started = Instant::now();
                let turn_result =
                    scope_turn_latency_counters(turn_latency_counters.clone(), async {
                        async {
                            let outcome = run_turn_once(&mut ctx).await?;
                            let mut current = load_object_state(&ctx).await?;
                            current.apply_turn_outcome(outcome);
                            persist_state(&ctx, &current);
                            sync_status(&ctx, session_id, &current).await?;
                            Ok::<TurnOutcome, HandlerError>(outcome)
                        }
                        .instrument(turn_root_span.clone())
                        .await
                    })
                    .await;

                let turn_latency_snapshot = turn_latency_counters.snapshot();
                record_turn_latency(turn_started.elapsed());
                emit_turn_latency_summary(&turn_root_span, turn_number, &turn_latency_snapshot);
                turn_result
            })
            .await?;
            let turn_snapshot = turn_counters.snapshot();
            emit_turn_replay_summary(&turn_root_span, turn_number, &turn_snapshot);

            match turn_outcome {
                TurnOutcome::Continue => continue,
                TurnOutcome::Idle | TurnOutcome::WaitingApproval | TurnOutcome::Cancelled => {
                    return Ok(Json::from(turn_outcome));
                }
            }
        }
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }
}

async fn load_object_state(ctx: &ObjectContext<'_>) -> Result<SessionVoState, HandlerError> {
    Ok(SessionVoState {
        meta: ctx
            .get::<Json<SessionMeta>>(K_META)
            .await?
            .map(Json::into_inner),
        status: ctx
            .get::<Json<SessionStatus>>(K_STATUS)
            .await?
            .map(Json::into_inner),
        pending: ctx
            .get::<Json<Vec<UserMessage>>>(K_PENDING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        pending_approval: ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner),
        children: ctx
            .get::<Json<Vec<SubAgentChildRef>>>(K_CHILDREN)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_turn_summary: ctx
            .get::<Json<String>>(K_LAST_TURN_SUMMARY)
            .await?
            .map(Json::into_inner),
        cancel_flag: ctx
            .get::<Json<CancelMode>>(K_CANCEL_FLAG)
            .await?
            .map(Json::into_inner),
    })
}

async fn load_shared_state(ctx: &SharedObjectContext<'_>) -> Result<SessionVoState, HandlerError> {
    Ok(SessionVoState {
        meta: ctx
            .get::<Json<SessionMeta>>(K_META)
            .await?
            .map(Json::into_inner),
        status: ctx
            .get::<Json<SessionStatus>>(K_STATUS)
            .await?
            .map(Json::into_inner),
        pending: ctx
            .get::<Json<Vec<UserMessage>>>(K_PENDING)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        pending_approval: ctx
            .get::<Json<String>>(K_PENDING_APPROVAL)
            .await?
            .map(Json::into_inner),
        children: ctx
            .get::<Json<Vec<SubAgentChildRef>>>(K_CHILDREN)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default(),
        last_turn_summary: ctx
            .get::<Json<String>>(K_LAST_TURN_SUMMARY)
            .await?
            .map(Json::into_inner),
        cancel_flag: ctx
            .get::<Json<CancelMode>>(K_CANCEL_FLAG)
            .await?
            .map(Json::into_inner),
    })
}

fn persist_state(ctx: &ObjectContext<'_>, state: &SessionVoState) {
    match &state.meta {
        Some(meta) => ctx.set(K_META, Json::from(meta.clone())),
        None => ctx.clear(K_META),
    }
    match &state.status {
        Some(status) => ctx.set(K_STATUS, Json::from(status.clone())),
        None => ctx.clear(K_STATUS),
    }
    if state.pending.is_empty() {
        ctx.clear(K_PENDING);
    } else {
        ctx.set(K_PENDING, Json::from(state.pending.clone()));
    }
    match &state.pending_approval {
        Some(pending_approval) => ctx.set(K_PENDING_APPROVAL, Json::from(pending_approval.clone())),
        None => ctx.clear(K_PENDING_APPROVAL),
    }
    if state.children.is_empty() {
        ctx.clear(K_CHILDREN);
    } else {
        ctx.set(K_CHILDREN, Json::from(state.children.clone()));
    }
    match &state.last_turn_summary {
        Some(summary) => ctx.set(K_LAST_TURN_SUMMARY, Json::from(summary.clone())),
        None => ctx.clear(K_LAST_TURN_SUMMARY),
    }
    match state.cancel_flag {
        Some(mode) => ctx.set(K_CANCEL_FLAG, Json::from(mode)),
        None => ctx.clear(K_CANCEL_FLAG),
    }
}

fn session_turn_span(meta: &SessionMeta, prompt: Option<&str>, turn_number: i64) -> tracing::Span {
    let environment = CONFIG
        .get()
        .and_then(|config| config.observability.environment.as_deref());
    let trace_name = TraceContext::from_session_meta(meta, prompt)
        .with_environment(environment.map(str::to_string))
        .trace_name
        .unwrap_or_else(|| format!("MOA turn {turn_number}"));
    let turn_root_span = tracing::info_span!(
        "session_turn",
        otel.name = %trace_name,
        moa.session.id = %meta.id,
        moa.workspace.id = %meta.workspace_id,
        moa.user.id = %meta.user_id,
        moa.turn.number = turn_number,
        moa.turn.get_events_calls = tracing::field::Empty,
        moa.turn.events_replayed = tracing::field::Empty,
        moa.turn.events_bytes = tracing::field::Empty,
        moa.turn.get_events_total_ms = tracing::field::Empty,
        moa.turn.snapshot_load_ms = tracing::field::Empty,
        moa.turn.snapshot_hit = tracing::field::Empty,
        moa.turn.snapshot_write_ms = tracing::field::Empty,
        moa.turn.pipeline_compile_ms = tracing::field::Empty,
        moa.turn.llm_call_ms = tracing::field::Empty,
        moa.turn.tool_dispatch_ms = tracing::field::Empty,
        moa.turn.event_persist_ms = tracing::field::Empty,
        moa.turn.llm_ttft_ms = tracing::field::Empty,
        moa.turn.compaction_tier1 = tracing::field::Empty,
        moa.turn.compaction_tier2 = tracing::field::Empty,
        moa.turn.compaction_tier3 = tracing::field::Empty,
        moa.turn.compaction_tokens_reclaimed = tracing::field::Empty,
        moa.turn.compaction_messages_elided = tracing::field::Empty,
        langfuse.trace.metadata.turn_number = turn_number,
    );
    apply_session_trace(&turn_root_span, meta, prompt, environment);
    add_session_trace_link(&turn_root_span, meta.id);
    turn_root_span
}

fn llm_call_span(meta: &SessionMeta) -> tracing::Span {
    if let Some(turn_root_span) = current_turn_root_span() {
        tracing::info_span!(
            parent: &turn_root_span,
            "llm_call",
            gen_ai.request.model = %meta.model,
            moa.session.id = %meta.id,
            moa.workspace.id = %meta.workspace_id,
            moa.user.id = %meta.user_id,
        )
    } else {
        tracing::info_span!(
            "llm_call",
            gen_ai.request.model = %meta.model,
            moa.session.id = %meta.id,
            moa.workspace.id = %meta.workspace_id,
            moa.user.id = %meta.user_id,
        )
    }
}

fn tool_dispatch_span(tool_name: &str) -> tracing::Span {
    if let Some(turn_root_span) = current_turn_root_span() {
        tracing::info_span!(
            parent: &turn_root_span,
            "tool_dispatch",
            moa.tool.name = tool_name,
        )
    } else {
        tracing::info_span!("tool_dispatch", moa.tool.name = tool_name)
    }
}

fn event_persist_span(events_written: usize) -> tracing::Span {
    if let Some(turn_root_span) = current_turn_root_span() {
        tracing::info_span!(
            parent: &turn_root_span,
            "event_persist",
            moa.persist.events_written = events_written as i64,
        )
    } else {
        tracing::info_span!(
            "event_persist",
            moa.persist.events_written = events_written as i64,
        )
    }
}

async fn persist_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<SessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
}

async fn run_turn_once(ctx: &mut ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let mut state = load_object_state(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(to_handler_error)?
        .clone();

    if state.cancel_flag.is_some() {
        return Ok(TurnOutcome::Cancelled);
    }
    if state.pending_approval.is_some() {
        return Ok(TurnOutcome::WaitingApproval);
    }

    if !state.pending.is_empty() {
        state.drain_pending_messages();
        persist_state(ctx, &state);
    }

    let prepared = ctx
        .run(|| async move {
            prepare_turn_request(session_id)
                .await
                .map(Json::from)
                .map_err(to_handler_error)
        })
        .name("prepare_turn_request")
        .await?
        .into_inner();

    let mut request = match prepared {
        PreparedTurnRequest::Idle => return Ok(TurnOutcome::Idle),
        PreparedTurnRequest::Request(request) => request,
    };
    ensure_dispatch_tool_schema(&mut request);

    let llm_call_span = llm_call_span(&meta);
    let llm_started = Instant::now();
    let response = ctx
        .service_client::<LLMGatewayClient>()
        .complete(Json::from(request))
        .call()
        .instrument(llm_call_span)
        .await?
        .into_inner();
    record_turn_llm_call_duration(llm_started.elapsed());

    update_last_turn_summary(ctx, &response).await?;

    for (index, tool_call) in response_tool_calls(&response).iter().enumerate() {
        if cancellation_requested(ctx).await? {
            return Ok(TurnOutcome::Cancelled);
        }

        let tool_id = stable_tool_call_id(session_id, index, tool_call);
        let invocation = ToolInvocation {
            id: tool_call.invocation.id.clone(),
            name: tool_call.invocation.name.clone(),
            input: tool_call.invocation.input.clone(),
        };

        if invocation.name == "dispatch_sub_agent" {
            persist_session_event(
                ctx,
                session_id,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    provider_thought_signature: tool_call
                        .provider_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.thought_signature())
                        .map(str::to_string),
                    tool_name: invocation.name.clone(),
                    input: invocation.input.clone(),
                    hand_id: None,
                },
            )
            .await?;

            let dispatch_input =
                serde_json::from_value(invocation.input.clone()).map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize dispatch_sub_agent input: {error}"
                    ))
                })?;
            let dispatch_span = tool_dispatch_span("dispatch_sub_agent");
            let dispatch_started = Instant::now();
            let dispatched = dispatch_sub_agent(
                ctx,
                K_CHILDREN,
                None,
                session_id,
                None,
                0,
                dispatch_input,
                meta.workspace_id.clone(),
                meta.user_id.clone(),
                meta.model.clone(),
            )
            .instrument(dispatch_span)
            .await?;
            record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);
            let output = sub_agent_result_tool_output(&dispatched.result);
            persist_session_event(
                ctx,
                session_id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    output: output.clone(),
                    original_output_tokens: output.original_output_tokens,
                    success: !output.is_error,
                    duration_ms: 0,
                },
            )
            .await?;
            continue;
        }

        let approval = ctx
            .service_client::<WorkspaceStoreClient>()
            .prepare_tool_approval(Json(PrepareToolApprovalRequest {
                session: meta.clone(),
                invocation: invocation.clone(),
                request_id: tool_id.0,
            }))
            .call()
            .await?
            .into_inner();

        if matches!(approval.action, PolicyAction::Deny) {
            persist_session_event(
                ctx,
                session_id,
                Event::ToolError {
                    tool_id,
                    provider_tool_use_id: invocation.id.clone(),
                    tool_name: invocation.name.clone(),
                    error: format!("tool {} denied by policy", invocation.name),
                    retryable: false,
                },
            )
            .await?;
            continue;
        }

        if matches!(approval.action, PolicyAction::RequireApproval) {
            let prompt = approval.prompt.ok_or_else(|| {
                TerminalError::new(format!(
                    "workspace store did not return an approval prompt for tool {}",
                    invocation.name
                ))
            })?;
            let (awakeable_id, awakeable) = ctx.awakeable::<String>();

            let mut waiting_state = load_object_state(ctx).await?;
            waiting_state.pending_approval = Some(awakeable_id.clone());
            waiting_state.set_status(SessionStatus::WaitingApproval);
            persist_state(ctx, &waiting_state);
            sync_status(ctx, session_id, &waiting_state).await?;

            persist_session_event(
                ctx,
                session_id,
                Event::ApprovalRequested {
                    request_id: prompt.request.request_id,
                    awakeable_id: Some(awakeable_id.clone()),
                    sub_agent_id: None,
                    tool_name: prompt.request.tool_name.clone(),
                    input_summary: prompt.request.input_summary.clone(),
                    risk_level: prompt.request.risk_level.clone(),
                    prompt: prompt.clone(),
                },
            )
            .await?;

            let approval_timeout = approval_wait_timeout();
            let timed_out_reason = format!(
                "Auto-denied: no decision within {} minutes",
                approval_timeout.as_secs() / 60
            );
            let approval_started = Instant::now();
            let decision = restate_sdk::select! {
                decision = awakeable => {
                    parse_awakeable_decision(&decision?)?
                },
                _ = ctx.sleep(approval_timeout) => {
                    ApprovalDecision::Deny {
                        reason: Some(timed_out_reason.clone()),
                    }
                }
            };
            record_approval_wait(
                approval_started.elapsed(),
                approval_outcome_label(&decision, &timed_out_reason),
            );

            let mut resumed_state = load_object_state(ctx).await?;
            resumed_state.pending_approval = None;
            resumed_state.set_status(SessionStatus::Running);
            persist_state(ctx, &resumed_state);
            sync_status(ctx, session_id, &resumed_state).await?;

            let decided_by = match &decision {
                ApprovalDecision::Deny {
                    reason: Some(reason),
                } if reason == &timed_out_reason => "system:auto-timeout".to_string(),
                _ => meta.user_id.to_string(),
            };

            persist_session_event(
                ctx,
                session_id,
                Event::ApprovalDecided {
                    request_id: prompt.request.request_id,
                    sub_agent_id: None,
                    decision: decision.clone(),
                    decided_by,
                    decided_at: Utc::now(),
                },
            )
            .await?;

            match decision {
                ApprovalDecision::AllowOnce => {}
                ApprovalDecision::AlwaysAllow { pattern } => {
                    ctx.service_client::<WorkspaceStoreClient>()
                        .store_approval_rule(Json(StoreApprovalRuleRequest {
                            session: meta.clone(),
                            tool_name: invocation.name.clone(),
                            pattern,
                            action: PolicyAction::Allow,
                            created_by: meta.user_id.clone(),
                        }))
                        .call()
                        .await?;
                }
                ApprovalDecision::Deny { reason } => {
                    let message = reason.unwrap_or_else(|| "Denied by the user".to_string());
                    let denied_output = ToolOutput {
                        content: vec![ToolContent::Text {
                            text: format!("Tool execution denied: {message}"),
                        }],
                        is_error: true,
                        structured: None,
                        duration: Duration::ZERO,
                        truncated: false,
                        original_output_tokens: None,
                        artifact: None,
                    };
                    persist_session_event(
                        ctx,
                        session_id,
                        Event::ToolResult {
                            tool_id,
                            provider_tool_use_id: invocation.id.clone(),
                            output: denied_output,
                            original_output_tokens: None,
                            success: false,
                            duration_ms: 0,
                        },
                    )
                    .await?;
                    continue;
                }
            }
        }

        let request = ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            session_id: Some(session_id),
            workspace_id: meta.workspace_id.clone(),
            user_id: meta.user_id.clone(),
            idempotency_key: invocation.id.clone(),
        };
        let tool_dispatch_span = tool_dispatch_span(&invocation.name);
        let tool_dispatch_started = Instant::now();
        ctx.service_client::<ToolExecutorClient>()
            .execute(Json::from(request))
            .call()
            .instrument(tool_dispatch_span)
            .await?;
        record_turn_tool_dispatch_duration(tool_dispatch_started.elapsed(), 1);
    }

    Ok(turn_outcome_for_response(&response))
}

async fn sync_status(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    state: &SessionVoState,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(0);
    let persist_started = Instant::now();
    ctx.service_client::<SessionStoreClient>()
        .update_status(Json(UpdateStatusRequest {
            session_id,
            status: state.current_status(),
        }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 0);
    Ok(())
}

fn parse_session_key(key: &str) -> Result<SessionId, HandlerError> {
    uuid::Uuid::parse_str(key)
        .map(SessionId)
        .map_err(|error| TerminalError::new(format!("invalid session key `{key}`: {error}")).into())
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

async fn update_last_turn_summary(
    ctx: &ObjectContext<'_>,
    response: &CompletionResponse,
) -> Result<(), HandlerError> {
    let mut state = load_object_state(ctx).await?;
    state.last_turn_summary = summarize_response_text(response);
    persist_state(ctx, &state);
    Ok(())
}

async fn cancellation_requested(ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
    Ok(load_object_state(ctx).await?.cancel_flag.is_some())
}

fn response_tool_calls(response: &CompletionResponse) -> Vec<&ToolCallContent> {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            CompletionContent::ToolCall(tool_call) => Some(tool_call),
            CompletionContent::Text(_) | CompletionContent::ProviderToolResult { .. } => None,
        })
        .collect()
}

fn turn_outcome_for_response(response: &CompletionResponse) -> TurnOutcome {
    if !response_tool_calls(response).is_empty() || response.stop_reason == StopReason::ToolUse {
        return TurnOutcome::Continue;
    }

    if response.stop_reason == StopReason::Cancelled {
        return TurnOutcome::Cancelled;
    }

    TurnOutcome::Idle
}

fn summarize_response_text(response: &CompletionResponse) -> Option<String> {
    let trimmed = response.text.trim();
    if trimmed.is_empty() {
        return None;
    }

    const MAX_SUMMARY_CHARS: usize = 240;
    Some(trimmed.chars().take(MAX_SUMMARY_CHARS).collect())
}

fn ensure_dispatch_tool_schema(request: &mut moa_core::CompletionRequest) {
    let has_dispatch_tool = request.tools.iter().any(|tool| {
        tool.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| name == "dispatch_sub_agent")
    });
    if !has_dispatch_tool {
        request.tools.push(dispatch_sub_agent_tool_schema());
    }
}

fn stable_tool_call_id(
    session_id: SessionId,
    index: usize,
    tool_call: &ToolCallContent,
) -> ToolCallId {
    if let Some(raw_id) = tool_call.invocation.id.as_deref()
        && let Ok(uuid) = Uuid::parse_str(raw_id)
    {
        return ToolCallId(uuid);
    }

    let mut left = DefaultHasher::new();
    "moa.session.turn.tool.left".hash(&mut left);
    session_id.hash(&mut left);
    index.hash(&mut left);
    tool_call.invocation.name.hash(&mut left);
    if let Some(raw_id) = tool_call.invocation.id.as_deref() {
        raw_id.hash(&mut left);
    }
    tool_call.invocation.input.to_string().hash(&mut left);
    let left = left.finish();

    let mut right = DefaultHasher::new();
    "moa.session.turn.tool.right".hash(&mut right);
    session_id.hash(&mut right);
    index.hash(&mut right);
    tool_call.invocation.name.hash(&mut right);
    if let Some(raw_id) = tool_call.invocation.id.as_deref() {
        raw_id.hash(&mut right);
    }
    tool_call.invocation.input.to_string().hash(&mut right);
    let right = right.finish();

    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&left.to_be_bytes());
    bytes[8..].copy_from_slice(&right.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    ToolCallId(Uuid::from_bytes(bytes))
}

fn approval_wait_timeout() -> Duration {
    const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

    approval_wait_timeout_from_env(
        std::env::var("MOA_APPROVAL_TIMEOUT_SECS").ok().as_deref(),
        DEFAULT_APPROVAL_TIMEOUT_SECS,
    )
}

fn approval_wait_timeout_from_env(raw: Option<&str>, default_secs: u64) -> Duration {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

fn approval_outcome_label(decision: &ApprovalDecision, timed_out_reason: &str) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "allow_once",
        ApprovalDecision::AlwaysAllow { .. } => "always_allow",
        ApprovalDecision::Deny {
            reason: Some(reason),
        } if reason == timed_out_reason => "timeout",
        ApprovalDecision::Deny { .. } => "deny",
    }
}

fn serialize_awakeable_decision(decision: &ApprovalDecision) -> Result<String, TerminalError> {
    serde_json::to_string(decision).map_err(|error| {
        TerminalError::new(format!(
            "failed to serialize approval decision for awakeable: {error}"
        ))
    })
}

fn parse_awakeable_decision(raw: &str) -> Result<ApprovalDecision, TerminalError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize approval decision from awakeable: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::{
        ApprovalDecision, Attachment, CompletionContent, CompletionResponse, ModelId, Platform,
        StopReason, TokenUsage, ToolCallContent, ToolInvocation, UserId, WorkspaceId,
    };
    use serde_json::json;

    use super::{
        SessionVoState, TurnOutcome, approval_wait_timeout_from_env, parse_awakeable_decision,
        serialize_awakeable_decision, stable_tool_call_id, summarize_response_text,
        turn_outcome_for_response,
    };

    fn test_message(text: &str) -> moa_core::UserMessage {
        moa_core::UserMessage {
            text: text.to_string(),
            attachments: vec![Attachment {
                name: "a.txt".to_string(),
                mime_type: Some("text/plain".to_string()),
                url: None,
                path: None,
                size_bytes: Some(3),
            }],
        }
    }

    fn test_meta() -> moa_core::SessionMeta {
        moa_core::SessionMeta {
            workspace_id: WorkspaceId::new("workspace-1"),
            user_id: UserId::new("user-1"),
            platform: Platform::Desktop,
            model: ModelId::new("test-model"),
            ..moa_core::SessionMeta::default()
        }
    }

    fn completion_response(
        text: &str,
        content: Vec<CompletionContent>,
        stop_reason: StopReason,
    ) -> CompletionResponse {
        CompletionResponse {
            text: text.to_string(),
            content,
            stop_reason,
            model: "test-model".into(),
            usage: TokenUsage {
                input_tokens_uncached: 12,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 6,
            },
            duration_ms: 10,
            thought_signature: None,
        }
    }

    #[test]
    fn session_vo_requires_meta_before_enqueue() {
        let mut state = SessionVoState::default();
        let error = state
            .enqueue_message(test_message("hello"))
            .expect_err("enqueue should fail without metadata");

        assert!(error.to_string().contains("Session metadata missing"));
    }

    #[test]
    fn session_vo_queues_messages_and_transitions_to_running() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"))
            .expect("enqueue should succeed");

        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.current_status(), moa_core::SessionStatus::Running);
    }

    #[test]
    fn session_vo_idle_turn_maps_to_paused_status() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        let status = state.apply_turn_outcome(TurnOutcome::Idle);

        assert_eq!(status, moa_core::SessionStatus::Paused);
        assert_eq!(state.current_status(), moa_core::SessionStatus::Paused);
    }

    #[test]
    fn session_vo_cancel_flag_round_trips() {
        let mut state = SessionVoState::default();
        state.set_cancel_flag(moa_core::CancelMode::Soft);

        assert_eq!(state.take_cancel_flag(), Some(moa_core::CancelMode::Soft));
        assert_eq!(state.take_cancel_flag(), None);
    }

    #[test]
    fn session_vo_destroy_clears_projection() {
        let mut state = SessionVoState::default();
        state.set_meta(test_meta());
        state
            .enqueue_message(test_message("hello"))
            .expect("enqueue should succeed");
        state.pending_approval = Some("approval-1".to_string());
        state.children.push(moa_core::SubAgentChildRef {
            id: "child-1".to_string(),
            task_hash: "hash-1".to_string(),
        });
        state.last_turn_summary = Some("summary".to_string());
        state.set_cancel_flag(moa_core::CancelMode::Hard);
        state.destroy();

        assert_eq!(state, SessionVoState::default());
    }

    #[test]
    fn session_vo_turn_outcome_and_approval_types_round_trip() {
        let outcome =
            serde_json::to_string(&TurnOutcome::WaitingApproval).expect("serialize turn outcome");
        let decision = serde_json::to_string(&ApprovalDecision::AllowOnce)
            .expect("serialize approval decision");

        assert!(outcome.contains("waiting_approval"));
        assert!(decision.contains("allow_once"));
    }

    #[test]
    fn tool_use_response_continues_the_session() {
        let response = completion_response(
            "working",
            vec![CompletionContent::ToolCall(ToolCallContent {
                invocation: ToolInvocation {
                    id: Some("provider-tool-id".to_string()),
                    name: "file_read".to_string(),
                    input: json!({"path":"/tmp/test.txt"}),
                },
                provider_metadata: None,
            })],
            StopReason::ToolUse,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Continue);
    }

    #[test]
    fn cancelled_response_maps_to_cancelled_outcome() {
        let response = completion_response(
            "",
            vec![CompletionContent::Text(String::new())],
            StopReason::Cancelled,
        );

        assert_eq!(turn_outcome_for_response(&response), TurnOutcome::Cancelled);
    }

    #[test]
    fn stable_tool_call_id_is_deterministic() {
        let session_id = moa_core::SessionId::new();
        let call = ToolCallContent {
            invocation: ToolInvocation {
                id: Some("provider-tool-id".to_string()),
                name: "bash".to_string(),
                input: json!({"command":"echo hello"}),
            },
            provider_metadata: None,
        };

        let first = stable_tool_call_id(session_id, 0, &call);
        let second = stable_tool_call_id(session_id, 0, &call);
        let third = stable_tool_call_id(session_id, 1, &call);

        assert_eq!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn summarize_response_text_trims_and_limits() {
        let response = completion_response(
            &"a".repeat(300),
            vec![CompletionContent::Text("ok".to_string())],
            StopReason::EndTurn,
        );

        let summary = summarize_response_text(&response).expect("summary should exist");
        assert_eq!(summary.len(), 240);
    }

    #[test]
    fn approval_timeout_defaults_when_override_is_missing_or_invalid() {
        assert_eq!(
            approval_wait_timeout_from_env(None, 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(
            approval_wait_timeout_from_env(Some("not-a-number"), 1800),
            Duration::from_secs(1800)
        );
        assert_eq!(
            approval_wait_timeout_from_env(Some("0"), 1800),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn approval_timeout_uses_positive_override() {
        assert_eq!(
            approval_wait_timeout_from_env(Some("45"), 1800),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn awakeable_decision_round_trips_through_json_payload() {
        let encoded = serialize_awakeable_decision(&ApprovalDecision::AlwaysAllow {
            pattern: "bash printf*".to_string(),
        })
        .expect("serialize approval decision");
        let decoded = parse_awakeable_decision(&encoded).expect("deserialize approval decision");

        assert_eq!(
            decoded,
            ApprovalDecision::AlwaysAllow {
                pattern: "bash printf*".to_string(),
            }
        );
    }
}
