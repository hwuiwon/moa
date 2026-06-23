//! TurnExecution workflow for running one session turn as a durable invocation.
//!
//! Keyed by `turn_id` so each turn has at most one in-flight workflow. The
//! Session VO will eventually fire `TurnExecution/run/send` and immediately
//! return; prompt 08 ports the existing in-process turn body into this workflow.
//!
//! Cancellation uses a durable workflow promise for the cancellation reason.
//! The workflow body races its work against that promise, while the shared
//! `request_cancel` handler only resolves the promise after checking the
//! persisted phase.
//!
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_brain::learning::{
    attribution::attributions_for_experience, candidates::propose_candidates_for_experience,
    experience::experience_from_assessment,
};
use moa_brain::lineage::emit_generation_lineage;
use moa_brain::pipeline::segments::SegmentTracker;
use moa_brain::segment_assessment::{
    AssessmentOverride, SegmentAssessor, continuation_signal, self_assessment_signal,
    structural_signal, tool_signal, verification_signal,
};
use moa_core::restate_observability::{
    annotate_restate_handler_span, emit_turn_latency_summary, emit_turn_replay_summary,
    event_persist_span, llm_call_span, session_turn_span, tool_dispatch_span,
};
use moa_core::wire::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest, GetEventsRequest,
    GetSegmentBaselineRequest, RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest,
    RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
    UpdateSegmentAssessmentRequest,
};
use moa_core::{
    ActionPolicyEffect, ActiveSegment, AssessmentPhase, CompletionRequest, CompletionResponse,
    Event, EventRange, EventRecord, EventType, LearningEntry, MoaError, ModelTier,
    QueryRewriteResult, SandboxFile, SegmentId, SessionId, SessionMeta, SessionStore as _,
    TaskSegment, ToolCallContent, ToolCallId, ToolCallRequest, ToolInvocation, ToolOutput,
    TurnLatencyCounters, TurnOutcome as CoreTurnOutcome, TurnReplayCounters,
    is_delegation_tool_name, record_session_error, record_turn_event_persist_duration,
    record_turn_latency, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
    record_turn_workflow_outcome, scope_turn_latency_counters, scope_turn_replay_counters,
};
use moa_lineage_citation::ChunkRef;
use moa_lineage_core::TurnId;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::brain_bridge::{PreparedTurnRequest, QueryRewriteCacheEntry, prepare_turn_request};
use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::{
    action_reviews::{ActionReviewsClient, RequestActionReview},
    llm_gateway::LLMGatewayClient,
    session_store::RestateSessionStoreClient,
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareActionReviewRequest, WorkspaceStoreClient},
};
use crate::turn::util::{
    allowed_tool_names, blocked_canary_tool_output, denied_tool_output, disallowed_tool_output,
    ensure_delegation_tool_schemas, meaningful_cancel_reason, response_tool_calls,
    stable_tool_call_id, summarize_response_text, tool_call_is_allowed, tool_input_leaks_canary,
    turn_outcome_for_response,
};
#[cfg(feature = "skill-learning")]
use crate::workflows::skill_learning::{RunSkillLearningRequest, SkillLearningClient};

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PHASE: &str = "phase";
const K_USER_MESSAGE_SEQUENCE: &str = "user_message_sequence";
const K_QUERY_REWRITE_CACHE: &str = "query_rewrite_cache";

#[derive(Clone, Debug)]
struct BodyOutcome {
    kind: TurnOutcomeKind,
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SegmentBoundarySequences {
    start_seq: u64,
    completed_seq: Option<u64>,
}

#[derive(Clone, Debug)]
struct BuiltTurnRequest {
    request: CompletionRequest,
    active_canary: Option<String>,
    trusted_sandbox_files: Vec<SandboxFile>,
    citation_sources: Vec<ChunkRef>,
}

/// Restate workflow surface for durable turn execution.
#[restate_sdk::workflow]
pub trait TurnExecution {
    /// Runs one turn workflow body.
    async fn run(request: Json<RunTurnRequest>) -> Result<Json<TurnOutcome>, HandlerError>;

    /// Requests cancellation of the in-flight turn.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Returns workflow progress without blocking the workflow body.
    #[shared]
    async fn progress() -> Result<Json<TurnProgress>, HandlerError>;
}

/// Concrete `TurnExecution` workflow implementation.
pub struct TurnExecutionImpl;

impl TurnExecution for TurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("TurnExecution", "run");
        let request = request.into_inner();
        let _runtime = OrchestratorCtx::current();

        ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));
        tracing::info!(
            session_id = %request.session_id,
            turn_id = %request.turn_id,
            "TurnExecution workflow started"
        );

        let session_id = parse_session_id(&request.session_id)?;
        let turn_id = parse_turn_id(&request.turn_id)?;
        let workflow_started = Instant::now();
        let outcome = match execute_turn_inside_workflow(&ctx, &request, session_id, turn_id).await
        {
            Ok(body) => {
                let phase = match body.kind {
                    TurnOutcomeKind::Completed => TurnPhase::Completed,
                    TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
                    TurnOutcomeKind::Failed => TurnPhase::Failed,
                };
                ctx.set(K_PHASE, Json::from(phase));
                TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: body.kind,
                    message: body.message,
                }
            }
            Err(err) => {
                ctx.set(K_PHASE, Json::from(TurnPhase::Failed));
                TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Failed,
                    message: format!("{err:?}"),
                }
            }
        };

        record_turn_workflow_outcome(
            "root",
            turn_outcome_kind_label(&outcome.kind),
            ModelTier::Main,
            workflow_started.elapsed(),
        );
        notify_session_of_outcome(&ctx, &request.session_id, &request.identity, &outcome);
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("TurnExecution", "request_cancel");
        let phase = ctx
            .get::<Json<TurnPhase>>(K_PHASE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        if is_terminal_phase(&phase) {
            return Ok(());
        }

        let Some(reason) = meaningful_cancel_reason(Some(reason.into_inner())) else {
            return Ok(());
        };
        ctx.resolve_promise(K_CANCEL_REASON_PROMISE, reason);
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        annotate_restate_handler_span("TurnExecution", "progress");
        let phase = ctx
            .get::<Json<TurnPhase>>(K_PHASE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let cancel_reason =
            meaningful_cancel_reason(ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?);
        Ok(Json::from(TurnProgress {
            turn_id: ctx.key().to_string(),
            phase,
            cancel_requested: cancel_reason.is_some(),
            cancel_reason,
        }))
    }
}

async fn execute_turn_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunTurnRequest,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<BodyOutcome, HandlerError> {
    if let Some(reason) = cancel_requested(ctx).await? {
        return Ok(BodyOutcome {
            kind: TurnOutcomeKind::Cancelled,
            message: reason,
        });
    }

    let user_sequence_num = append_session_event(
        ctx,
        session_id,
        Event::UserMessage {
            text: request.user_message.clone(),
            attachments: request.attachments.clone(),
        },
    )
    .await?;
    ctx.set(K_USER_MESSAGE_SEQUENCE, Json::from(user_sequence_num));
    ctx.clear(K_QUERY_REWRITE_CACHE);

    let max_turns = OrchestratorCtx::current_config().session_limits.max_turns;
    let max_turns = if max_turns == 0 {
        usize::MAX
    } else {
        max_turns as usize
    };
    let mut last_summary = None;

    for turn_number in 1..=max_turns {
        if let Some(reason) = cancel_requested(ctx).await? {
            return Ok(BodyOutcome {
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        let meta = load_session_meta(ctx, session_id).await.ok();
        let turn_root_span = create_turn_span(
            meta.as_ref(),
            Some(request.user_message.as_str()),
            turn_number,
        );
        let turn_counters = Arc::new(TurnReplayCounters::default());
        let turn_outcome = scope_turn_replay_counters(turn_counters.clone(), async {
            let turn_latency_counters = Arc::new(TurnLatencyCounters::new(turn_root_span.clone()));
            let turn_started = Instant::now();
            let turn_result = scope_turn_latency_counters(turn_latency_counters.clone(), async {
                run_once_inside_workflow(
                    ctx,
                    session_id,
                    turn_id,
                    &request.identity,
                    &mut last_summary,
                )
                .instrument(turn_root_span.clone())
                .await
            })
            .await;

            let turn_latency_snapshot = turn_latency_counters.snapshot();
            record_turn_latency(turn_started.elapsed());
            emit_turn_latency_summary(&turn_root_span, turn_number as i64, &turn_latency_snapshot);
            turn_result
        })
        .await?;
        let turn_snapshot = turn_counters.snapshot();
        emit_turn_replay_summary(&turn_root_span, turn_number as i64, &turn_snapshot);

        match turn_outcome {
            CoreTurnOutcome::Continue => continue,
            CoreTurnOutcome::Cancelled => {
                assess_current_active_segment(
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[AssessmentOverride::Cancelled],
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Cancelled,
                    message: last_summary
                        .take()
                        .unwrap_or_else(|| "turn cancelled by provider".to_string()),
                });
            }
            CoreTurnOutcome::Idle => {
                assess_current_active_segment(ctx, session_id, AssessmentPhase::Final, &[]).await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message: last_summary.unwrap_or_else(|| "idle".to_string()),
                });
            }
        }
    }

    assess_current_active_segment(
        ctx,
        session_id,
        AssessmentPhase::Final,
        &[AssessmentOverride::TurnBudgetExceeded],
    )
    .await?;
    emit_turn_budget_exceeded(ctx, session_id, max_turns).await?;
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Completed,
        message: format!("turn budget exceeded ({max_turns}), stopping"),
    })
}

async fn run_once_inside_workflow(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: TurnId,
    identity: &moa_core::traits::Identity,
    last_summary: &mut Option<String>,
) -> Result<CoreTurnOutcome, HandlerError> {
    if let Some(reason) = cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(CoreTurnOutcome::Cancelled);
    }

    ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));
    let Some(built_request) = build_request_inside_workflow(ctx, session_id, turn_id).await? else {
        return Ok(CoreTurnOutcome::Idle);
    };
    let BuiltTurnRequest {
        mut request,
        active_canary,
        trusted_sandbox_files,
        citation_sources,
    } = built_request;
    if let Some(reason) = cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(CoreTurnOutcome::Cancelled);
    }

    let meta = load_session_meta(ctx, session_id).await?;
    OrchestratorCtx::current_tool_router()
        .set_trusted_sandbox_files(&meta, trusted_sandbox_files)
        .await;
    let active_segment = ensure_current_segment(ctx, session_id, &meta, &mut request).await?;
    if let Some(segment) = active_segment.as_ref() {
        request.metadata.insert(
            "_moa.segment_id".to_string(),
            serde_json::json!(segment.id.to_string()),
        );
        request.metadata.insert(
            "_moa.segment_index".to_string(),
            serde_json::json!(segment.segment_index),
        );
    }
    ensure_delegation_tool_schemas(&mut request);
    let allowed_tools = allowed_tool_names(&request);
    let request_model = request
        .model
        .as_ref()
        .map(|model| model.as_str())
        .unwrap_or(meta.model.as_str())
        .to_string();

    ctx.set(K_PHASE, Json::from(TurnPhase::Streaming));
    let span = llm_call_span(&meta);
    let llm_started = Instant::now();
    let response = {
        let _guard = span.enter();
        restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                *last_summary = Some(reason);
                return Ok(CoreTurnOutcome::Cancelled);
            },
            response = ctx
                .service_client::<LLMGatewayClient>()
                .complete(Json::from(request))
                .call() => {
                    response?.into_inner()
            }
        }
    };
    let llm_call_duration = llm_started.elapsed();
    record_turn_llm_call_duration(llm_call_duration);
    let response_usage = response.token_usage();
    let response_cost_cents =
        crate::services::llm_gateway::compute_cost_cents(response.model.as_str(), response_usage);
    let response_event =
        latest_matching_brain_response_event(ctx, session_id, identity, &response).await?;
    let lineage = OrchestratorCtx::current_lineage();
    emit_generation_lineage(
        lineage.as_ref(),
        turn_id,
        &meta,
        "llm_gateway",
        &request_model,
        &response,
        &citation_sources,
        response_cost_cents,
        llm_call_duration,
        &span,
        response_event.as_ref(),
    )
    .await;

    record_response(ctx, session_id, &response, last_summary).await?;

    for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
        if let Some(reason) = cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(CoreTurnOutcome::Cancelled);
        }
        handle_tool_call(
            ctx,
            &meta,
            session_id,
            &allowed_tools,
            index,
            tool_call,
            active_canary.as_deref(),
        )
        .await?;
    }

    Ok(turn_outcome_for_response(&response))
}

async fn build_request_inside_workflow(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<Option<BuiltTurnRequest>, HandlerError> {
    let active_user_sequence_num = ctx
        .get::<Json<u64>>(K_USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner);
    let cached_query_rewrite = ctx
        .get::<Json<QueryRewriteCacheEntry>>(K_QUERY_REWRITE_CACHE)
        .await?
        .map(Json::into_inner);
    let prepared = ctx
        .run(|| async move {
            prepare_turn_request(
                session_id,
                turn_id,
                active_user_sequence_num,
                cached_query_rewrite,
            )
            .await
            .map(Json::from)
            .map_err(to_handler_error)
        })
        .name("prepare_turn_request")
        .await?
        .into_inner();
    if let Some(cache) = prepared.query_rewrite_cache {
        ctx.set(K_QUERY_REWRITE_CACHE, Json::from(cache));
    } else {
        ctx.clear(K_QUERY_REWRITE_CACHE);
    }

    Ok(match prepared.prepared {
        PreparedTurnRequest::Idle => None,
        PreparedTurnRequest::Request(request) => Some(BuiltTurnRequest {
            request: *request,
            active_canary: prepared.active_canary,
            trusted_sandbox_files: prepared.trusted_sandbox_files,
            citation_sources: prepared.citation_sources,
        }),
    })
}

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    allowed_tools: &std::collections::BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
    active_canary: Option<&str>,
) -> Result<(), HandlerError> {
    ctx.set(K_PHASE, Json::from(TurnPhase::Tooling));
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let invocation = tool_call.invocation.clone();

    if !tool_call_is_allowed(allowed_tools, &invocation.name) {
        append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
        let output = disallowed_tool_output(&invocation.name);
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        return Ok(());
    }

    if is_delegation_tool_name(&invocation.name) {
        handle_delegation_tool(ctx, meta, session_id, tool_id, tool_call).await?;
        return Ok(());
    }

    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let prepared_action = ctx
        .service_client::<WorkspaceStoreClient>()
        .prepare_action_review(Json(PrepareActionReviewRequest {
            session: meta.clone(),
            invocation: invocation.clone(),
            review_id: tool_id.0,
            tool_call_id: tool_id,
            sub_agent_id: None,
            origin_kind: None,
            origin_id: None,
            origin_step_id: None,
            idempotency_key: invocation.id.clone(),
        }))
        .call()
        .await?
        .into_inner();

    if matches!(prepared_action.effect, ActionPolicyEffect::Deny) {
        let reason = prepared_action
            .reason
            .as_deref()
            .unwrap_or("denied by action policy");
        let output = denied_tool_output(format!(
            "Tool {} denied by action policy: {reason}",
            invocation.name
        ));
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        return Ok(());
    }

    if matches!(prepared_action.effect, ActionPolicyEffect::AdminReview) {
        let tool_request = ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: active_canary.map(ToOwned::to_owned),
            session_id: Some(session_id),
            tenant_id: meta.tenant_id,
            user_id: storage_user_id(meta),
            idempotency_key: invocation.id.clone(),
        };
        if tool_input_leaks_canary(active_canary, &tool_request.input).map_err(to_handler_error)? {
            let output = blocked_canary_tool_output(&invocation.name);
            append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
            return Ok(());
        }
        ctx.service_client::<ActionReviewsClient>()
            .request(Json::from(RequestActionReview {
                envelope: prepared_action.envelope,
                preview: prepared_action.preview,
                tool_request,
            }))
            .call()
            .await?;
        let output = ToolOutput::error(
            format!(
                "Action is pending tenant admin review: {}: {}",
                invocation.name, prepared_action.input_summary
            ),
            Duration::ZERO,
        );
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        return Ok(());
    }

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = ctx
        .service_client::<ToolExecutorClient>()
        .execute(Json::from(ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: active_canary.map(ToOwned::to_owned),
            session_id: Some(session_id),
            tenant_id: meta.tenant_id,
            user_id: storage_user_id(meta),
            idempotency_key: invocation.id.clone(),
        }))
        .call()
        .instrument(span)
        .await?
        .into_inner();
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    let Some(tool) =
        moa_core::DelegationTool::from_invocation(&invocation).map_err(to_handler_error)?
    else {
        return Err(
            TerminalError::new(format!("unsupported delegation tool {}", invocation.name)).into(),
        );
    };

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession { session_id, meta },
        tool,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_session_event(
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

    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn ensure_current_segment(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    request: &mut CompletionRequest,
) -> Result<Option<ActiveSegment>, HandlerError> {
    let active_segment = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(session_id))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view());

    let now = durable_utc_now(ctx).await?;
    let mut active_segment = active_segment;
    if let Some(transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        &tenant_key(meta),
        &active_segment,
        now,
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
            assess_completed_segment_at_transition(
                ctx,
                session_id,
                meta,
                &completed,
                &request.metadata,
            )
            .await?;
        }

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

        active_segment = Some(transition.active_segment);
    }

    Ok(active_segment)
}

async fn assess_completed_segment_at_transition(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    completed: &moa_brain::pipeline::segments::SegmentCompleted,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    if !OrchestratorCtx::current_config().resolution.enabled {
        return Ok(());
    }

    let boundaries = load_segment_boundary_events(ctx, session_id).await?;
    let (segment_events, next_user_message) = if let Some(boundary) =
        segment_boundary_sequences(&boundaries, completed.segment_id)
    {
        let next_user = load_next_user_message_cutoff(ctx, session_id, boundary.start_seq)
            .await?
            .map(|(text, sequence_num)| (Some(text), Some(sequence_num)))
            .unwrap_or((None, None));
        let events = load_segment_assessment_events(
            ctx,
            session_id,
            completed.segment_id,
            boundary,
            next_user.1,
            true,
        )
        .await?;
        (
            segment_events_for_assessment(&events, completed.segment_id, next_user.1),
            next_user.0,
        )
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %completed.segment_id,
            "segment start event missing; falling back to full event log for completed segment assessment"
        );
        let events = load_session_events_fallback(ctx, session_id).await?;
        let (next_user_message, next_user_seq) = latest_user_message(&events)
            .map(|(text, sequence_num)| (Some(text.to_string()), Some(sequence_num)))
            .unwrap_or((None, None));
        let segment_events =
            segment_events_for_assessment(&events, completed.segment_id, next_user_seq);
        (segment_events, next_user_message)
    };
    let rewrite = query_rewrite_from_metadata(metadata);
    let baseline = load_segment_baseline(ctx, meta.tenant_id).await?;
    let phase = if next_user_message.is_some() {
        AssessmentPhase::Deferred
    } else {
        AssessmentPhase::Immediate
    };
    let assessment = assess_segment_events(
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

    record_segment_assessment_learning(ctx, meta.tenant_id, completed.segment_id, &assessment)
        .await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_assessment(Json(UpdateSegmentAssessmentRequest {
            segment_id: completed.segment_id,
            assessment: assessment.clone(),
        }))
        .call()
        .await?;
    let segment = task_segment_from_completed(meta, completed, &segment_events, &assessment);
    emit_experience_for_assessment(
        ctx,
        meta,
        &segment,
        &assessment,
        &segment_events,
        rewrite.as_ref(),
        Some(completed.duration_ms),
    )
    .await?;
    Ok(())
}

async fn assess_current_active_segment(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    phase: AssessmentPhase,
    overrides: &[AssessmentOverride],
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config().resolution.enabled {
        return Ok(());
    }

    let meta = load_session_meta(ctx, session_id).await?;
    let Some(segment) = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(session_id))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view())
    else {
        return Ok(());
    };

    let boundaries = load_segment_boundary_events(ctx, session_id).await?;
    let segment_events = if let Some(boundary) = segment_boundary_sequences(&boundaries, segment.id)
    {
        let events =
            load_segment_assessment_events(ctx, session_id, segment.id, boundary, None, true)
                .await?;
        segment_events_for_assessment(&events, segment.id, None)
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %segment.id,
            "segment start event missing; falling back to full event log for active segment assessment"
        );
        let events = load_session_events_fallback(ctx, session_id).await?;
        segment_events_for_assessment(&events, segment.id, None)
    };
    let baseline = load_segment_baseline(ctx, meta.tenant_id).await?;
    let duration_ms = durable_utc_now(ctx)
        .await?
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    let assessment = assess_segment_events(
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

    record_segment_assessment_learning(ctx, meta.tenant_id, segment.id, &assessment).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_assessment(Json(UpdateSegmentAssessmentRequest {
            segment_id: segment.id,
            assessment: assessment.clone(),
        }))
        .call()
        .await?;
    let task_segment = task_segment_from_active(&meta, &segment, &assessment, None);
    emit_experience_for_assessment(
        ctx,
        &meta,
        &task_segment,
        &assessment,
        &segment_events,
        None,
        Some(duration_ms),
    )
    .await?;
    Ok(())
}

fn task_segment_from_completed(
    meta: &SessionMeta,
    completed: &moa_brain::pipeline::segments::SegmentCompleted,
    events: &[EventRecord],
    assessment: &moa_core::SegmentAssessment,
) -> TaskSegment {
    let (started_at, previous_segment_id) = events
        .iter()
        .find_map(|record| match &record.event {
            Event::SegmentStarted {
                segment_id,
                previous_segment_id,
                ..
            } if *segment_id == completed.segment_id => {
                Some((record.timestamp, *previous_segment_id))
            }
            _ => None,
        })
        .unwrap_or((assessment.assessed_at, None));
    TaskSegment {
        id: completed.segment_id,
        session_id: meta.id,
        tenant_id: tenant_key(meta),
        segment_index: completed.segment_index,
        task_summary: completed.task_summary.clone(),
        started_at,
        ended_at: Some(assessment.assessed_at),
        turn_count: completed.turn_count,
        tools_used: completed.tools_used.clone(),
        skills_activated: completed.skills_activated.clone(),
        token_cost: completed.token_cost,
        previous_segment_id,
        outcome: Some(assessment.outcome.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    }
}

fn task_segment_from_active(
    meta: &SessionMeta,
    segment: &ActiveSegment,
    assessment: &moa_core::SegmentAssessment,
    ended_at: Option<DateTime<Utc>>,
) -> TaskSegment {
    TaskSegment {
        id: segment.id,
        session_id: meta.id,
        tenant_id: tenant_key(meta),
        segment_index: segment.segment_index,
        task_summary: segment.task_summary.clone(),
        started_at: segment.started_at,
        ended_at,
        turn_count: segment.turn_count,
        tools_used: segment.tools_used.clone(),
        skills_activated: segment.skills_activated.clone(),
        token_cost: segment.token_cost,
        previous_segment_id: None,
        outcome: Some(assessment.outcome.as_str().to_string()),
        assessment: Some(assessment.clone()),
        outcome_confidence: Some(assessment.confidence),
    }
}

fn tenant_key(meta: &SessionMeta) -> String {
    meta.tenant_id.to_string()
}

fn storage_user_id(meta: &SessionMeta) -> moa_core::UserId {
    let value = meta
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .or_else(|| meta.created_by.as_ref().map(session_actor_storage_id))
        .unwrap_or_else(|| format!("tenant:{}", meta.tenant_id));
    moa_core::UserId::new(value)
}

fn session_actor_storage_id(actor: &moa_core::SessionActorRef) -> String {
    match actor {
        moa_core::SessionActorRef::Identity { id } => format!("identity:{id}"),
        moa_core::SessionActorRef::Contact { id } => id.to_string(),
        moa_core::SessionActorRef::Anonymous => "anonymous".to_string(),
    }
}

async fn emit_experience_for_assessment(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    segment: &TaskSegment,
    assessment: &moa_core::SegmentAssessment,
    segment_events: &[EventRecord],
    rewrite: Option<&QueryRewriteResult>,
    duration_ms: Option<u64>,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx).await?;
    let experience = experience_from_assessment(
        meta,
        segment,
        assessment,
        segment_events,
        rewrite,
        duration_ms,
        now,
    );
    let attributions = attributions_for_experience(&experience, segment_events, now);
    let candidates = propose_candidates_for_experience(&experience, &attributions, now);
    let runtime = OrchestratorCtx::current();
    let store = runtime.session_store();
    #[cfg(feature = "skill-learning")]
    let experience_id = experience.id;
    #[cfg(feature = "skill-learning")]
    let min_skill_learning_tool_calls = runtime.config().learning.skills.min_tool_calls;
    let learning_error = ctx
        .run(move || {
            let store = store.clone();
            let experience = experience.clone();
            let attributions = attributions.clone();
            let candidates = candidates.clone();
            async move {
                let result = async {
                    store.append_experience_record(&experience).await?;
                    store.append_experience_attributions(&attributions).await?;
                    for candidate in &candidates {
                        store.append_learning_candidate(candidate).await?;
                    }
                    store.refresh_segment_materialized_views().await?;
                    Ok::<(), MoaError>(())
                }
                .await;
                Ok::<_, HandlerError>(Json::from(result.err().map(|error| error.to_string())))
            }
        })
        .name("emit_experience_learning")
        .await?
        .into_inner();

    if let Some(error) = learning_error {
        tracing::warn!(
            session_id = %meta.id,
            segment_id = %segment.id,
            error,
            "experience learning emission failed"
        );
        append_session_event(
            ctx,
            meta.id,
            Event::Warning {
                message: format!(
                    "experience learning emission failed for segment {}: {error}",
                    segment.id
                ),
            },
        )
        .await?;
        return Ok(());
    }
    #[cfg(feature = "skill-learning")]
    if skill_learning_dispatch_is_eligible(segment_events, min_skill_learning_tool_calls) {
        dispatch_skill_learning_after_experience(ctx, meta.id, experience_id).await?;
    }
    Ok(())
}

#[cfg(feature = "skill-learning")]
fn skill_learning_dispatch_is_eligible(
    segment_events: &[EventRecord],
    min_tool_calls: usize,
) -> bool {
    segment_events
        .iter()
        .filter(|record| matches!(record.event, Event::ToolCall { .. }))
        .count()
        >= min_tool_calls
}

#[cfg(feature = "skill-learning")]
async fn dispatch_skill_learning_after_experience(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    experience_id: uuid::Uuid,
) -> Result<(), HandlerError> {
    ctx.workflow_client::<SkillLearningClient>(experience_id.to_string())
        .run(Json(RunSkillLearningRequest {
            session_id,
            experience_id,
        }))
        .send();
    tracing::debug!(
        session_id = %session_id,
        experience_id = %experience_id,
        "dispatched detached skill learning workflow"
    );
    Ok(())
}

async fn record_segment_assessment_learning(
    ctx: &WorkflowContext<'_>,
    tenant_id: moa_core::TenantId,
    segment_id: SegmentId,
    assessment: &moa_core::SegmentAssessment,
) -> Result<(), HandlerError> {
    let session_store = OrchestratorCtx::current_session_store();
    let assessment = assessment.clone();
    ctx.run(|| async move {
        session_store
            .append_learning(&LearningEntry {
                id: uuid::Uuid::now_v7(),
                tenant_id,
                learning_type: "segment_assessed".to_string(),
                target_id: segment_id.to_string(),
                target_label: Some(assessment.outcome.as_str().to_string()),
                payload: serde_json::to_value(&assessment).map_err(|error| {
                    HandlerError::from(MoaError::StorageError(format!(
                        "serialize segment assessment learning payload: {error}"
                    )))
                })?,
                confidence: Some(assessment.confidence),
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
    .name("record_segment_assessment_learning")
    .await?;
    Ok(())
}

async fn load_events_in_range(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    range: EventRange,
    operation_name: &'static str,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(move || {
            let store = store.clone();
            let range = range.clone();
            async move {
                store
                    .get_events(session_id, range)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            }
        })
        .name(operation_name)
        .await?
        .into_inner())
}

async fn load_segment_boundary_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    load_events_in_range(
        ctx,
        session_id,
        EventRange {
            event_types: Some(vec![EventType::SegmentStarted, EventType::SegmentCompleted]),
            ..EventRange::default()
        },
        "turn_execution_load_segment_boundaries",
    )
    .await
}

async fn load_segment_assessment_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    segment_id: SegmentId,
    boundary: SegmentBoundarySequences,
    cutoff_before_seq: Option<u64>,
    stop_at_completion: bool,
) -> Result<Vec<EventRecord>, HandlerError> {
    let to_seq = segment_assessment_to_seq(boundary, cutoff_before_seq, stop_at_completion);
    tracing::debug!(
        session_id = %session_id,
        segment_id = %segment_id,
        from_seq = boundary.start_seq,
        to_seq = ?to_seq,
        "loading bounded events for segment assessment"
    );
    load_events_in_range(
        ctx,
        session_id,
        EventRange {
            from_seq: Some(boundary.start_seq),
            to_seq,
            ..EventRange::default()
        },
        "turn_execution_load_segment_assessment_events",
    )
    .await
}

async fn load_next_user_message_cutoff(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    segment_start_seq: u64,
) -> Result<Option<(String, u64)>, HandlerError> {
    let current_user_sequence = ctx
        .get::<Json<u64>>(K_USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > segment_start_seq);
    if let Some(sequence_num) = current_user_sequence {
        let events = load_events_in_range(
            ctx,
            session_id,
            EventRange {
                from_seq: Some(sequence_num),
                to_seq: Some(sequence_num),
                event_types: Some(vec![EventType::UserMessage]),
                ..EventRange::default()
            },
            "turn_execution_load_current_user_message",
        )
        .await?;
        if let Some((text, sequence_num)) = latest_user_message(&events) {
            return Ok(Some((text.to_string(), sequence_num)));
        }
        tracing::warn!(
            session_id = %session_id,
            sequence_num,
            "current user message sequence was not found during completed segment assessment"
        );
    }

    let events = load_events_in_range(
        ctx,
        session_id,
        EventRange {
            from_seq: Some(segment_start_seq.saturating_add(1)),
            event_types: Some(vec![EventType::UserMessage]),
            ..EventRange::default()
        },
        "turn_execution_load_segment_user_messages",
    )
    .await?;
    Ok(latest_user_message(&events).map(|(text, sequence_num)| (text.to_string(), sequence_num)))
}

async fn load_session_events_fallback(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(move || {
            let store = store.clone();
            async move {
                store
                    .get_events(session_id, EventRange::all())
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            }
        })
        .name("turn_execution_load_session_events_fallback")
        .await?
        .into_inner())
}

fn segment_boundary_sequences(
    boundary_events: &[EventRecord],
    segment_id: SegmentId,
) -> Option<SegmentBoundarySequences> {
    let mut start_seq = None;
    let mut completed_seq = None;
    for record in boundary_events {
        match &record.event {
            Event::SegmentStarted {
                segment_id: started_id,
                ..
            } if *started_id == segment_id && start_seq.is_none() => {
                start_seq = Some(record.sequence_num);
            }
            Event::SegmentCompleted {
                segment_id: completed_id,
                ..
            } if *completed_id == segment_id && completed_seq.is_none() => {
                completed_seq = Some(record.sequence_num);
            }
            _ => {}
        }
    }

    start_seq.map(|start_seq| SegmentBoundarySequences {
        start_seq,
        completed_seq,
    })
}

fn segment_assessment_to_seq(
    boundary: SegmentBoundarySequences,
    cutoff_before_seq: Option<u64>,
    stop_at_completion: bool,
) -> Option<u64> {
    if let Some(sequence_num) = cutoff_before_seq {
        return Some(sequence_num.saturating_sub(1));
    }
    if stop_at_completion {
        return boundary.completed_seq;
    }
    None
}

async fn load_segment_baseline(
    ctx: &WorkflowContext<'_>,
    tenant_id: moa_core::TenantId,
) -> Result<Option<moa_core::SegmentBaseline>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_segment_baseline(Json(GetSegmentBaselineRequest { tenant_id }))
        .call()
        .await?
        .into_inner())
}

#[allow(clippy::too_many_arguments)]
fn assess_segment_events(
    segment_events: &[EventRecord],
    turn_count: u32,
    token_cost: u64,
    duration_ms: u64,
    baseline: Option<&moa_core::SegmentBaseline>,
    next_user_message: Option<&str>,
    is_new_task: bool,
    phase: AssessmentPhase,
    extra_overrides: &[AssessmentOverride],
) -> moa_core::SegmentAssessment {
    let config = OrchestratorCtx::current_config().resolution.clone();
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
        overrides.push(AssessmentOverride::AllToolsFailed);
    }

    SegmentAssessor::new(config.weights).assess(
        tool,
        verification,
        continuation,
        self_assessment,
        structural,
        phase,
        &overrides,
    )
}

fn segment_events_for_assessment(
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

async fn record_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    response: &CompletionResponse,
    last_summary: &mut Option<String>,
) -> Result<(), HandlerError> {
    *last_summary = summarize_response_text(response);
    let usage = response.token_usage();
    let token_cost = (usage.total_input_tokens() + usage.output_tokens) as u64;
    if token_cost > 0 {
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_turn_usage(Json(RecordSegmentTurnUsageRequest {
                session_id,
                token_cost,
            }))
            .send();
    }
    Ok(())
}

async fn latest_matching_brain_response_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    identity: &moa_core::traits::Identity,
    response: &CompletionResponse,
) -> Result<Option<EventRecord>, HandlerError> {
    let request = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_events(Json(GetEventsRequest {
            session_id,
            range: EventRange::recent(8),
        }));
    let events = with_identity_headers(request, identity)
        .call()
        .await?
        .into_inner();
    Ok(events
        .into_iter()
        .filter(|record| match &record.event {
            Event::BrainResponse { text, model, .. } => {
                text == &response.text && model == &response.model
            }
            _ => false,
        })
        .max_by_key(|record| record.sequence_num))
}

async fn record_segment_tool_use(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
            session_id,
            tool_name: tool_name.to_string(),
        }))
        .send();
    Ok(())
}

async fn emit_turn_budget_exceeded(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    max_turns: usize,
) -> Result<(), HandlerError> {
    record_session_error("turn_budget");
    append_session_event(
        ctx,
        session_id,
        Event::Error {
            message: format!("turn budget exceeded ({max_turns}), stopping"),
            recoverable: true,
        },
    )
    .await
    .map(|_| ())
}

async fn append_tool_call_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_session_event(
        ctx,
        session_id,
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: invocation.id,
            provider_thought_signature: tool_call
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.thought_signature())
                .map(str::to_string),
            tool_name: invocation.name,
            input: invocation.input,
            hand_id: None,
        },
    )
    .await
    .map(|_| ())
}

async fn append_tool_result_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
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
    .map(|_| ())
}

async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    let sequence_num = ctx
        .service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest { session_id, event }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(sequence_num)
}

async fn load_session_meta(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<SessionMeta, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    Ok(ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("turn_execution_load_session_meta")
        .await?
        .into_inner())
}

fn create_turn_span(
    meta: Option<&SessionMeta>,
    prompt: Option<&str>,
    turn_number: usize,
) -> tracing::Span {
    let Some(meta) = meta else {
        return tracing::info_span!(
            "session_turn",
            otel.name = %format!("MOA turn {turn_number}"),
            moa.turn.number = turn_number as i64,
        );
    };
    session_turn_span(
        meta,
        prompt,
        turn_number as i64,
        OrchestratorCtx::current_config()
            .observability
            .environment
            .as_deref(),
    )
}

async fn durable_utc_now(ctx: &WorkflowContext<'_>) -> Result<DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("workflow_utc_now")
        .await?
        .into_inner())
}

fn parse_session_id(raw: &str) -> Result<SessionId, HandlerError> {
    uuid::Uuid::parse_str(raw)
        .map(SessionId)
        .map_err(|error| TerminalError::new(format!("invalid session_id `{raw}`: {error}")).into())
}

fn parse_turn_id(raw: &str) -> Result<TurnId, HandlerError> {
    uuid::Uuid::parse_str(raw)
        .map(TurnId)
        .map_err(|error| TerminalError::new(format!("invalid turn_id `{raw}`: {error}")).into())
}

async fn cancel_requested(ctx: &WorkflowContext<'_>) -> Result<Option<String>, HandlerError> {
    Ok(meaningful_cancel_reason(
        ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?,
    ))
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

fn notify_session_of_outcome(
    ctx: &WorkflowContext<'_>,
    session_id: &str,
    identity: &moa_core::traits::Identity,
    outcome: &TurnOutcome,
) {
    let request = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .record_turn_outcome(Json::from(outcome.clone()));
    with_identity_headers(request, identity).send();
    tracing::info!(
        session_id = %session_id,
        turn_id = %outcome.turn_id,
        kind = ?outcome.kind,
        "TurnExecution outcome notified to Session VO"
    );
}

fn is_terminal_phase(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Cancelled | TurnPhase::Failed
    )
}

fn turn_outcome_kind_label(kind: &TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{Event, EventRecord, SegmentId, SessionId};
    use uuid::Uuid;

    use super::{
        SegmentBoundarySequences, segment_assessment_to_seq, segment_boundary_sequences,
        segment_events_for_assessment,
    };

    fn event_record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        let event_type = event.event_type();
        EventRecord {
            id: Uuid::from_u128(sequence_num as u128),
            session_id,
            sequence_num,
            event_type,
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    fn segment_started(
        session_id: SessionId,
        sequence_num: u64,
        segment_id: SegmentId,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::SegmentStarted {
                segment_id,
                segment_index: 0,
                task_summary: Some("target task".to_string()),
                previous_segment_id: None,
            },
        )
    }

    fn segment_completed(
        session_id: SessionId,
        sequence_num: u64,
        segment_id: SegmentId,
    ) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::SegmentCompleted {
                segment_id,
                segment_index: 0,
                task_summary: Some("target task".to_string()),
                turn_count: 1,
                tools_used: Vec::new(),
                skills_activated: Vec::new(),
                token_cost: 10,
                duration_ms: 50,
            },
        )
    }

    fn user_message(session_id: SessionId, sequence_num: u64, text: &str) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::UserMessage {
                text: text.to_string(),
                attachments: Vec::new(),
            },
        )
    }

    fn warning(session_id: SessionId, sequence_num: u64, message: &str) -> EventRecord {
        event_record(
            session_id,
            sequence_num,
            Event::Warning {
                message: message.to_string(),
            },
        )
    }

    #[test]
    fn segment_boundary_sequences_match_requested_segment_only() {
        // Pins: boundary lookup uses durable segment boundary events for the target segment.
        let session_id = SessionId::new();
        let target_segment = SegmentId::new();
        let other_segment = SegmentId::new();
        let boundaries = vec![
            segment_started(session_id, 2, other_segment),
            segment_started(session_id, 10, target_segment),
            segment_completed(session_id, 18, other_segment),
            segment_completed(session_id, 31, target_segment),
        ];

        assert_eq!(
            segment_boundary_sequences(&boundaries, target_segment),
            Some(SegmentBoundarySequences {
                start_seq: 10,
                completed_seq: Some(31),
            })
        );
        assert_eq!(
            segment_boundary_sequences(&boundaries, SegmentId::new()),
            None
        );
    }

    #[test]
    fn segment_assessment_to_seq_prefers_next_user_cutoff() {
        // Pins: completed segment assessment ends before the next user message when known.
        let boundary = SegmentBoundarySequences {
            start_seq: 10,
            completed_seq: Some(40),
        };

        assert_eq!(
            segment_assessment_to_seq(boundary, Some(35), true),
            Some(34)
        );
        assert_eq!(segment_assessment_to_seq(boundary, None, true), Some(40));
        assert_eq!(segment_assessment_to_seq(boundary, None, false), None);
    }

    #[test]
    fn segment_events_for_assessment_starts_at_segment_and_stops_before_cutoff() {
        // Pins: segment assessment excludes prior events and the next task's user message.
        let session_id = SessionId::new();
        let target_segment = SegmentId::new();
        let events = vec![
            user_message(session_id, 1, "previous task"),
            segment_started(session_id, 2, target_segment),
            user_message(session_id, 3, "target task"),
            warning(session_id, 4, "inside target segment"),
            user_message(session_id, 5, "next task"),
            segment_completed(session_id, 6, target_segment),
            warning(session_id, 7, "after target segment"),
        ];

        let filtered = segment_events_for_assessment(&events, target_segment, Some(5));
        let sequences = filtered
            .iter()
            .map(|record| record.sequence_num)
            .collect::<Vec<_>>();

        assert_eq!(sequences, vec![2, 3, 4]);
    }
}
