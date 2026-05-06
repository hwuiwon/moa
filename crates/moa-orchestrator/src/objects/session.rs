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

const K_META: &str = "meta";
const K_STATUS: &str = "status";
const K_PENDING: &str = "pending";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_CHILDREN: &str = "children";
const K_LAST_TURN_SUMMARY: &str = "last_turn_summary";
const K_CANCEL_FLAG: &str = "cancel_flag";
const K_CURRENT_SEGMENT: &str = "current_segment";
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
    /// Active task segment, when one has been created for the session.
    pub current_segment: Option<ActiveSegment>,
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

    /// Replaces the active task segment.
    pub fn set_current_segment(&mut self, segment: ActiveSegment) {
        self.current_segment = Some(segment);
    }

    /// Records a tool usage on the active task segment.
    pub fn record_segment_tool_use(&mut self, tool_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.tools_used.iter().any(|tool| tool == tool_name) {
            segment.tools_used.push(tool_name.to_string());
        }
    }

    /// Records one completed model turn on the active task segment.
    pub fn record_segment_turn_usage(&mut self, token_cost: u64) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        segment.turn_count = segment.turn_count.saturating_add(1);
        segment.token_cost = segment.token_cost.saturating_add(token_cost);
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

impl VoState for SessionVoState {
    async fn load_from<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(Self {
            meta: reader.get_json(K_META).await?,
            status: reader.get_json(K_STATUS).await?,
            pending: reader.get_json(K_PENDING).await?.unwrap_or_default(),
            pending_approval: reader.get_json(K_PENDING_APPROVAL).await?,
            children: reader.get_json(K_CHILDREN).await?.unwrap_or_default(),
            last_turn_summary: reader.get_json(K_LAST_TURN_SUMMARY).await?,
            cancel_flag: reader.get_json(K_CANCEL_FLAG).await?,
            current_segment: reader.get_json(K_CURRENT_SEGMENT).await?,
        })
    }

    fn persist_into(&self, ctx: &ObjectContext<'_>) {
        set_or_clear_opt(ctx, K_META, self.meta.as_ref());
        set_or_clear_opt(ctx, K_STATUS, self.status.as_ref());
        set_or_clear_vec(ctx, K_PENDING, &self.pending);
        set_or_clear_opt(ctx, K_PENDING_APPROVAL, self.pending_approval.as_ref());
        set_or_clear_vec(ctx, K_CHILDREN, &self.children);
        set_or_clear_opt(ctx, K_LAST_TURN_SUMMARY, self.last_turn_summary.as_ref());
        set_or_clear_opt(ctx, K_CANCEL_FLAG, self.cancel_flag.as_ref());
        set_or_clear_opt(ctx, K_CURRENT_SEGMENT, self.current_segment.as_ref());
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

pub(crate) struct SessionTurnAdapter;

impl AgentAdapter for SessionTurnAdapter {
    fn children_state_key(&self) -> &'static str {
        K_CHILDREN
    }

    fn sub_agent_id(&self, _ctx: &ObjectContext<'_>) -> Option<SubAgentId> {
        None
    }

    async fn is_cancelled(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SessionVoState::load_from(ctx).await?.cancel_flag.is_some())
    }

    async fn has_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<bool, HandlerError> {
        Ok(SessionVoState::load_from(ctx)
            .await?
            .pending_approval
            .is_some())
    }

    async fn build_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<CompletionRequest>, HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
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
        Ok(match prepared {
            PreparedTurnRequest::Idle => None,
            PreparedTurnRequest::Request(request) => {
                let mut request = *request;
                ensure_current_segment(ctx, session_id, &mut request).await?;
                Some(request)
            }
        })
    }

    async fn session_meta(&self, ctx: &ObjectContext<'_>) -> Result<SessionMeta, HandlerError> {
        SessionVoState::load_from(ctx)
            .await?
            .meta
            .ok_or_else(|| TerminalError::new("session meta missing").into())
    }

    async fn turn_prompt(&self, ctx: &ObjectContext<'_>) -> Result<Option<String>, HandlerError> {
        Ok(SessionVoState::load_from(ctx)
            .await?
            .pending
            .last()
            .map(|message| message.text.clone()))
    }

    async fn owning_session_id(&self, ctx: &ObjectContext<'_>) -> Result<SessionId, HandlerError> {
        parse_session_key(ctx.key())
    }

    async fn apply_outcome(
        &self,
        ctx: &ObjectContext<'_>,
        outcome: TurnOutcome,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let mut state = SessionVoState::load_from(ctx).await?;
        if matches!(outcome, TurnOutcome::Cancelled) {
            state.take_cancel_flag();
        }
        let is_cancelled = matches!(outcome, TurnOutcome::Cancelled);
        let is_idle = matches!(outcome, TurnOutcome::Idle);
        state.apply_turn_outcome(outcome);
        if is_cancelled {
            if let Some(segment) = state.current_segment.as_ref() {
                score_active_segment(
                    ctx,
                    session_id,
                    &state,
                    segment,
                    ScoringPhase::Final,
                    &[ResolutionOverride::Cancelled],
                )
                .await?;
            }
        } else if is_idle && let Some(segment) = state.current_segment.as_ref() {
            score_active_segment(
                ctx,
                session_id,
                &state,
                segment,
                ScoringPhase::Immediate,
                &[],
            )
            .await?;
        }
        state.persist_into(ctx);
        sync_status(ctx, session_id, &state).await
    }

    async fn emit_turn_budget_exceeded(
        &self,
        ctx: &ObjectContext<'_>,
        max_turns: usize,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        let state = SessionVoState::load_from(ctx).await?;
        if let Some(segment) = state.current_segment.as_ref() {
            score_active_segment(
                ctx,
                session_id,
                &state,
                segment,
                ScoringPhase::Final,
                &[ResolutionOverride::TurnBudgetExceeded],
            )
            .await?;
        }
        record_session_error("turn_budget");
        persist_session_event(
            ctx,
            session_id,
            Event::Error {
                message: format!("turn budget exceeded ({max_turns}), stopping"),
                recoverable: true,
            },
        )
        .await
    }

    async fn record_response(
        &self,
        ctx: &ObjectContext<'_>,
        response: &CompletionResponse,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.last_turn_summary = summarize_response_text(response);
        let usage = response.token_usage();
        let token_cost = (usage.total_input_tokens() + usage.output_tokens) as u64;
        state.record_segment_turn_usage(token_cost);
        state.persist_into(ctx);
        if token_cost > 0 {
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                    session_id: parse_session_key(ctx.key())?,
                    token_cost,
                }))
                .send();
        }
        Ok(())
    }

    async fn current_segment(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<Option<ActiveSegment>, HandlerError> {
        Ok(SessionVoState::load_from(ctx).await?.current_segment)
    }

    async fn record_segment_tool_use(
        &self,
        ctx: &ObjectContext<'_>,
        tool_name: &str,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.record_segment_tool_use(tool_name);
        state.persist_into(ctx);
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id: parse_session_key(ctx.key())?,
                tool_name: tool_name.to_string(),
            }))
            .send();
        Ok(())
    }

    async fn record_tool_result(
        &self,
        _ctx: &ObjectContext<'_>,
        _tool_id: ToolCallId,
        _invocation: &ToolInvocation,
        _output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        Ok(())
    }

    async fn record_denied_tool(
        &self,
        ctx: &ObjectContext<'_>,
        tool_id: ToolCallId,
        invocation: &ToolInvocation,
        output: &ToolOutput,
    ) -> Result<(), HandlerError> {
        let session_id = parse_session_key(ctx.key())?;
        persist_session_event(
            ctx,
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: invocation.id.clone(),
                output: output.clone(),
                original_output_tokens: output.original_output_tokens,
                success: false,
                duration_ms: 0,
            },
        )
        .await
    }

    async fn drain_pending_before_request(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        if !state.pending.is_empty() {
            state.drain_pending_messages();
            state.persist_into(ctx);
        }
        Ok(())
    }

    async fn dispatch_child(
        &self,
        ctx: &mut ObjectContext<'_>,
        input: DispatchSubAgentInput,
    ) -> Result<DispatchedSubAgent, HandlerError> {
        let meta = self.session_meta(ctx).await?;
        dispatch_sub_agent(
            ctx,
            self.children_state_key(),
            self.budget_state_key(),
            meta.id,
            self.sub_agent_id(ctx),
            0,
            input,
            meta.workspace_id,
            meta.user_id,
            meta.model,
        )
        .await
    }

    async fn set_pending_approval(
        &self,
        ctx: &ObjectContext<'_>,
        awakeable_id: String,
    ) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.pending_approval = Some(awakeable_id);
        state.set_status(SessionStatus::WaitingApproval);
        state.persist_into(ctx);
        sync_status(ctx, parse_session_key(ctx.key())?, &state).await
    }

    async fn clear_pending_approval(&self, ctx: &ObjectContext<'_>) -> Result<(), HandlerError> {
        let mut state = SessionVoState::load_from(ctx).await?;
        state.pending_approval = None;
        state.set_status(SessionStatus::Running);
        state.persist_into(ctx);
        sync_status(ctx, parse_session_key(ctx.key())?, &state).await
    }
}

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    async fn set_meta(
        &self,
        ctx: ObjectContext<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "set_meta");
        let mut state = SessionVoState::load_from(&ctx).await?;
        state.set_meta(meta.into_inner());
        state.persist_into(&ctx);
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
        let mut state = SessionVoState::load_from(&ctx).await?;
        let should_start_turn_runner = !matches!(
            state.current_status(),
            SessionStatus::Running | SessionStatus::WaitingApproval
        );
        state
            .enqueue_message(msg.clone())
            .map_err(to_handler_error)?;
        state.persist_into(&ctx);

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
        let mut state = SessionVoState::load_from(&ctx).await?;
        state.set_cancel_flag(mode.into_inner());
        let children = state.children.clone();
        state.persist_into(&ctx);
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
        Ok(Json::from(
            SessionVoState::load_from(&ctx).await?.current_status(),
        ))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn run_turn(
        &self,
        mut ctx: ObjectContext<'_>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("Session", "run_turn");
        let runner = TurnRunner::new(SessionTurnAdapter);
        let outcome = runner.run_until_idle(&mut ctx, MAX_TURNS_PER_POST).await?;
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Session", "destroy");
        ctx.clear_all();
        tracing::info!(key = %ctx.key(), "session VO state cleared");
        Ok(())
    }
}

async fn ensure_current_segment(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    request: &mut CompletionRequest,
) -> Result<(), HandlerError> {
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .meta
        .clone()
        .ok_or_else(|| TerminalError::new("session meta missing"))?;

    if state.current_segment.is_none()
        && let Some(segment) = ctx
            .service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(session_id))
            .call()
            .await?
            .into_inner()
    {
        state.current_segment = Some(segment.active_view());
    }

    if let Some(mut transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        meta.workspace_id.as_str(),
        &state.current_segment,
        Utc::now(),
    ) {
        if let Some(completed) = transition.completed.clone() {
            ctx.service_client::<RestateSessionStoreClient>()
                .complete_segment(Json(CompleteSegmentRequest {
                    segment_id: completed.segment_id,
                    update: completed.update.clone(),
                }))
                .send();
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event: completed.clone().into_event(),
                }))
                .send();
            score_completed_segment_at_transition(
                ctx,
                session_id,
                meta.workspace_id.as_str(),
                &completed,
                &request.metadata,
            )
            .await?;
        }

        classify_started_segment(ctx, meta.workspace_id.as_str(), request, &mut transition).await?;

        ctx.service_client::<RestateSessionStoreClient>()
            .create_segment(Json(CreateSegmentRequest {
                segment: transition.task_segment.clone(),
            }))
            .send();
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: transition.started.clone().into_event(),
            }))
            .send();

        state.set_current_segment(transition.active_segment);
        state.persist_into(ctx);
    }

    if let Some(segment) = state.current_segment.as_ref() {
        request.metadata.insert(
            "_moa.segment_id".to_string(),
            serde_json::json!(segment.id.to_string()),
        );
        request.metadata.insert(
            "_moa.segment_index".to_string(),
            serde_json::json!(segment.segment_index),
        );
    }

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct IntentClassification {
    label: String,
    confidence: f64,
}

async fn classify_started_segment(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    request: &CompletionRequest,
    transition: &mut moa_brain::pipeline::segments::SegmentTransition,
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config.intents.enabled {
        return Ok(());
    }
    let Some(embedding_provider) = runtime.embedding_provider.clone() else {
        return Ok(());
    };

    let session_store = runtime.session_store.clone();
    let threshold = runtime.config.intents.classification_threshold;
    let tenant_id = tenant_id.to_string();
    let task_summary = transition
        .task_segment
        .task_summary
        .clone()
        .unwrap_or_default();
    let first_user_message = user_message_for_intent(request).unwrap_or_default();
    let segment_id = transition.task_segment.id.0;

    let classification = ctx
        .run(|| async move {
            let classifier = IntentClassifier::with_threshold(
                session_store.clone(),
                embedding_provider,
                threshold,
            );
            let Some((intent, confidence)) = classifier
                .classify(&tenant_id, &task_summary, &first_user_message)
                .await
                .map_err(HandlerError::from)?
            else {
                return Ok(Json::from(None::<IntentClassification>));
            };

            session_store
                .append_learning(&LearningEntry {
                    id: uuid::Uuid::now_v7(),
                    tenant_id: tenant_id.clone(),
                    learning_type: "intent_classified".to_string(),
                    target_id: segment_id.to_string(),
                    target_label: Some(intent.label.clone()),
                    payload: serde_json::json!({
                        "intent_id": intent.id,
                        "task_summary": task_summary,
                        "first_user_message": first_user_message,
                    }),
                    confidence: Some(confidence),
                    source_refs: vec![segment_id],
                    actor: "system".to_string(),
                    valid_from: Utc::now(),
                    valid_to: None,
                    batch_id: None,
                    version: 1,
                })
                .await
                .map_err(HandlerError::from)?;

            Ok(Json::from(Some(IntentClassification {
                label: intent.label,
                confidence,
            })))
        })
        .name("classify_started_segment")
        .await?
        .into_inner();

    if let Some(classification) = classification {
        transition.task_segment.intent_label = Some(classification.label.clone());
        transition.task_segment.intent_confidence = Some(classification.confidence);
        transition.started.intent_label = Some(classification.label.clone());
        transition.started.intent_confidence = Some(classification.confidence);
        transition.active_segment.intent_label = Some(classification.label);
    }

    Ok(())
}

fn user_message_for_intent(request: &CompletionRequest) -> Option<String> {
    request
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.trim().to_string())
        .filter(|message| !message.is_empty())
}

async fn persist_session_event(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
}

async fn sync_status(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    state: &SessionVoState,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(0);
    let persist_started = Instant::now();
    ctx.service_client::<RestateSessionStoreClient>()
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

async fn score_completed_segment_at_transition(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    tenant_id: &str,
    completed: &moa_brain::pipeline::segments::SegmentCompleted,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    if !OrchestratorCtx::current().config.resolution.enabled {
        return Ok(());
    }

    let events = load_session_events(ctx, session_id).await?;
    let (next_user_message, next_user_seq) = latest_user_message(&events)
        .map(|(text, sequence_num)| (Some(text.to_string()), Some(sequence_num)))
        .unwrap_or((None, None));
    let segment_events = segment_events_for_scoring(&events, completed.segment_id, next_user_seq);
    let rewrite = query_rewrite_from_metadata(metadata);
    let baseline = load_segment_baseline(ctx, tenant_id, completed.intent_label.as_deref()).await?;
    let phase = if next_user_message.is_some() {
        ScoringPhase::Deferred
    } else {
        ScoringPhase::Immediate
    };
    let score = score_segment_events(
        &segment_events,
        completed.turn_count,
        completed.token_cost,
        completed.duration_ms,
        baseline.as_ref(),
        next_user_message.as_deref(),
        rewrite.as_ref().is_some_and(|rewrite| rewrite.is_new_task),
        phase,
        &[],
    );

    record_resolution_learning(ctx, tenant_id, completed.segment_id, &score).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_resolution_score(Json(UpdateSegmentResolutionScoreRequest {
            segment_id: completed.segment_id,
            score,
        }))
        .send();
    Ok(())
}

async fn score_active_segment(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    state: &SessionVoState,
    segment: &ActiveSegment,
    phase: ScoringPhase,
    overrides: &[ResolutionOverride],
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config.resolution.enabled {
        return Ok(());
    }
    let tenant_id = state
        .meta
        .as_ref()
        .map(|meta| meta.workspace_id.as_str())
        .ok_or_else(|| TerminalError::new("session meta missing"))?;
    let events = load_session_events(ctx, session_id).await?;
    let segment_events = segment_events_for_scoring(&events, segment.id, None);
    let baseline = load_segment_baseline(ctx, tenant_id, segment.intent_label.as_deref()).await?;
    let duration_ms = Utc::now()
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    let score = score_segment_events(
        &segment_events,
        segment.turn_count,
        segment.token_cost,
        duration_ms,
        baseline.as_ref(),
        None,
        false,
        phase,
        overrides,
    );

    record_resolution_learning(ctx, tenant_id, segment.id, &score).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_resolution_score(Json(UpdateSegmentResolutionScoreRequest {
            segment_id: segment.id,
            score,
        }))
        .send();
    Ok(())
}

async fn record_resolution_learning(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    segment_id: SegmentId,
    score: &moa_core::ResolutionScore,
) -> Result<(), HandlerError> {
    let session_store = OrchestratorCtx::current().session_store.clone();
    let tenant_id = tenant_id.to_string();
    let score = score.clone();
    ctx.run(|| async move {
        session_store
            .append_learning(&LearningEntry {
                id: uuid::Uuid::now_v7(),
                tenant_id,
                learning_type: "resolution_scored".to_string(),
                target_id: segment_id.to_string(),
                target_label: Some(score.label.as_str().to_string()),
                payload: serde_json::to_value(&score).map_err(|error| {
                    HandlerError::from(MoaError::StorageError(format!(
                        "serialize resolution score learning payload: {error}"
                    )))
                })?,
                confidence: Some(score.confidence),
                source_refs: vec![segment_id.0],
                actor: "system".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: None,
                version: 1,
            })
            .await
            .map_err(HandlerError::from)
    })
    .name("record_resolution_learning")
    .await?;
    Ok(())
}

async fn load_session_events(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_events(Json(GetEventsRequest {
            session_id,
            range: EventRange::all(),
        }))
        .call()
        .await?
        .into_inner())
}

async fn load_segment_baseline(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    intent_label: Option<&str>,
) -> Result<Option<moa_core::SegmentBaseline>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_segment_baseline(Json(GetSegmentBaselineRequest {
            tenant_id: tenant_id.to_string(),
            intent_label: intent_label.map(ToOwned::to_owned),
        }))
        .call()
        .await?
        .into_inner())
}

#[allow(clippy::too_many_arguments)]
fn score_segment_events(
    segment_events: &[EventRecord],
    turn_count: u32,
    token_cost: u64,
    duration_ms: u64,
    baseline: Option<&moa_core::SegmentBaseline>,
    next_user_message: Option<&str>,
    is_new_task: bool,
    phase: ScoringPhase,
    extra_overrides: &[ResolutionOverride],
) -> moa_core::ResolutionScore {
    let config = OrchestratorCtx::current().config.resolution.clone();
    let tool = tool_signal::score(segment_events);
    let verification = verification_signal::score(segment_events);
    let continuation = continuation_signal::score(
        continuation_signal::ContinuationInput {
            next_user_message,
            initial_query: first_user_message(segment_events),
            is_new_task,
        },
        config.rephrase_similarity_threshold,
    );
    let self_assessment = self_assessment_signal::score(last_brain_response(segment_events));
    let structural = structural_signal::score(
        structural_signal::SegmentMetrics {
            turn_count,
            token_cost,
            duration_secs: duration_ms as f64 / 1_000.0,
        },
        baseline,
        config.structural_min_samples,
    );
    let mut overrides = extra_overrides.to_vec();
    if let Some(override_value) = verification_signal::override_for_events(segment_events) {
        overrides.push(override_value);
    }
    if tool_signal::all_tools_failed(segment_events) {
        overrides.push(ResolutionOverride::AllToolsFailed);
    }

    ResolutionScorer::new(config.weights).score(
        tool,
        verification,
        continuation,
        self_assessment,
        structural,
        phase,
        &overrides,
    )
}

fn segment_events_for_scoring(
    events: &[EventRecord],
    segment_id: SegmentId,
    cutoff_before_seq: Option<u64>,
) -> Vec<EventRecord> {
    let start_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentStarted {
            segment_id: started_id,
            ..
        } if *started_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let completed_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentCompleted {
            segment_id: completed_id,
            ..
        } if *completed_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let end_exclusive = cutoff_before_seq
        .or_else(|| completed_seq.map(|sequence_num| sequence_num.saturating_add(1)));

    events
        .iter()
        .filter(|record| start_seq.is_none_or(|start_seq| record.sequence_num >= start_seq))
        .filter(|record| end_exclusive.is_none_or(|end_seq| record.sequence_num < end_seq))
        .cloned()
        .collect()
}

fn latest_user_message(events: &[EventRecord]) -> Option<(&str, u64)> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some((text.as_str(), record.sequence_num)),
        _ => None,
    })
}

fn first_user_message(events: &[EventRecord]) -> Option<&str> {
    events.iter().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn last_brain_response(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn query_rewrite_from_metadata(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<QueryRewriteResult> {
    metadata
        .get("query_rewrite")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

#[cfg(test)]
mod tests {
    use moa_core::{ApprovalDecision, Attachment, ModelId, Platform, UserId, WorkspaceId};

    use super::{SessionVoState, TurnOutcome};

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
    fn session_vo_current_segment_serializes() {
        let mut state = SessionVoState::default();
        let session_id = moa_core::SessionId::new();
        state.current_segment = Some(moa_core::ActiveSegment {
            id: moa_core::deterministic_segment_id(session_id, 0),
            segment_index: 0,
            intent_label: Some("coding".to_string()),
            task_summary: Some("Fix failing tests".to_string()),
            started_at: chrono::Utc::now(),
            tools_used: vec!["bash".to_string()],
            skills_activated: vec!["moa-rust".to_string()],
            turn_count: 1,
            token_cost: 123,
        });

        let json = serde_json::to_string(&state).expect("serialize session state");
        let decoded: SessionVoState =
            serde_json::from_str(&json).expect("deserialize session state");

        assert_eq!(decoded.current_segment, state.current_segment);
    }
}
