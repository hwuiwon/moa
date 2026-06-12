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
use moa_brain::pipeline::segments::SegmentTracker;
use moa_brain::resolution::{
    ResolutionOverride, ResolutionScorer, continuation_signal, self_assessment_signal,
    structural_signal, tool_signal, verification_signal,
};
use moa_core::restate_observability::{
    annotate_restate_handler_span, emit_turn_latency_summary, emit_turn_replay_summary,
    event_persist_span, llm_call_span, session_turn_span, tool_dispatch_span,
};
use moa_core::wire::{RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress};
use moa_core::{
    ActiveSegment, ApprovalDecision, ApprovalPrompt, CancelSubAgentInput, CompletionRequest,
    CompletionResponse, DispatchSubAgentInput, Event, EventRange, EventRecord, EventType,
    LearningEntry, ListSubAgentsInput, ListSubAgentsOutput, MessageSubAgentInput, MoaError,
    PolicyAction, QueryRewriteResult, ScoringPhase, SegmentId, SessionId, SessionMeta,
    SessionStatus, SessionStore as _, SpawnSubAgentInput, SpawnSubAgentOutput, SubAgentChildRef,
    ToolCallContent, ToolCallId, ToolCallRequest, ToolInvocation, ToolOutput, TurnLatencyCounters,
    TurnOutcome as CoreTurnOutcome, TurnReplayCounters, WaitSubAgentInput, WaitSubAgentOutput,
    is_delegation_tool_name, record_approval_wait, record_session_error,
    record_turn_event_persist_duration, record_turn_latency, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration, scope_turn_latency_counters, scope_turn_replay_counters,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::brain_bridge::{PreparedTurnRequest, QueryRewriteCacheEntry, prepare_turn_request};
use crate::objects::session::SessionClient;
use crate::objects::sub_agent::SubAgentClient;
use crate::services::{
    llm_gateway::LLMGatewayClient,
    session_store::{
        AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest,
        GetSegmentBaselineRequest, RecordSegmentToolUseRequest, RecordSegmentTurnUsageRequest,
        RestateSessionStoreClient, UpdateSegmentResolutionScoreRequest, UpdateStatusRequest,
    },
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareToolApprovalRequest, StoreApprovalRuleRequest, WorkspaceStoreClient},
};
use crate::sub_agent_dispatch::{
    DispatchedSubAgent, child_agent_path, child_is_owned, sub_agent_result_tool_output,
    validate_dispatch_budget, validate_dispatch_limits,
};
use crate::turn::approval::{parse_awakeable_decision, serialize_awakeable_decision};
use crate::turn::util::{
    allowed_tool_names, denied_tool_output, disallowed_tool_output, ensure_delegation_tool_schemas,
    ensure_dispatch_tool_schema, meaningful_cancel_reason, response_tool_calls,
    stable_tool_call_id, summarize_response_text, tool_call_is_allowed, turn_outcome_for_response,
};
use crate::workflows::approval_wait;

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PENDING_APPROVAL: &str = "pending_approval";
const K_PHASE: &str = "phase";
const K_USER_MESSAGE_SEQUENCE: &str = "user_message_sequence";
const K_QUERY_REWRITE_CACHE: &str = "query_rewrite_cache";
const APPROVAL_TIMEOUT_SECS_ENV: &str = "MOA_APPROVAL_TIMEOUT_SECS";
const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingApprovalState {
    awakeable_id: String,
    request_id: uuid::Uuid,
    session_id: SessionId,
    sub_agent_id: Option<String>,
}

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
        let outcome = match run_turn_inside_workflow(&ctx, &request, session_id).await {
            Ok(body) => {
                let phase = match body.kind {
                    TurnOutcomeKind::Completed => TurnPhase::Completed,
                    TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
                    TurnOutcomeKind::Failed => TurnPhase::Failed,
                };
                if matches!(body.kind, TurnOutcomeKind::Cancelled) {
                    cleanup_pending_approval_after_cancel(&ctx, session_id, &body.message).await?;
                }
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

        notify_session_of_outcome(&ctx, &request.session_id, &outcome);
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

async fn run_turn_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunTurnRequest,
    session_id: SessionId,
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

    let max_turns = OrchestratorCtx::current().config.session_limits.max_turns;
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
                run_once_inside_workflow(ctx, session_id, &mut last_summary)
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
                score_current_active_segment(
                    ctx,
                    session_id,
                    ScoringPhase::Final,
                    &[ResolutionOverride::Cancelled],
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Cancelled,
                    message: last_summary
                        .take()
                        .unwrap_or_else(|| "turn cancelled by provider".to_string()),
                });
            }
            CoreTurnOutcome::WaitingApproval => {
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Failed,
                    message: "turn unexpectedly returned while waiting for approval".to_string(),
                });
            }
            CoreTurnOutcome::Idle => {
                score_current_active_segment(ctx, session_id, ScoringPhase::Final, &[]).await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message: last_summary.unwrap_or_else(|| "idle".to_string()),
                });
            }
        }
    }

    score_current_active_segment(
        ctx,
        session_id,
        ScoringPhase::Final,
        &[ResolutionOverride::TurnBudgetExceeded],
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
    last_summary: &mut Option<String>,
) -> Result<CoreTurnOutcome, HandlerError> {
    if let Some(reason) = cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(CoreTurnOutcome::Cancelled);
    }

    ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));
    let Some(built_request) = build_request_inside_workflow(ctx, session_id).await? else {
        return Ok(CoreTurnOutcome::Idle);
    };
    let BuiltTurnRequest {
        mut request,
        active_canary,
    } = built_request;
    if let Some(reason) = cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(CoreTurnOutcome::Cancelled);
    }

    let meta = load_session_meta(ctx, session_id).await?;
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
    ensure_dispatch_tool_schema(&mut request);
    ensure_delegation_tool_schemas(&mut request);
    let allowed_tools = allowed_tool_names(&request);

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
    record_turn_llm_call_duration(llm_started.elapsed());

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
            prepare_turn_request(session_id, active_user_sequence_num, cached_query_rewrite)
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

    if invocation.name == "dispatch_sub_agent" {
        handle_dispatch(ctx, meta, session_id, tool_id, tool_call).await?;
        return Ok(());
    }

    if is_delegation_tool_name(&invocation.name) {
        handle_delegation_tool(ctx, meta, session_id, tool_id, tool_call).await?;
        return Ok(());
    }

    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let policy = ctx
        .service_client::<WorkspaceStoreClient>()
        .prepare_tool_approval(Json(PrepareToolApprovalRequest {
            session: meta.clone(),
            invocation: invocation.clone(),
            request_id: tool_id.0,
        }))
        .call()
        .await?
        .into_inner();

    if matches!(policy.action, PolicyAction::Deny) {
        append_session_event(
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
        return Ok(());
    }

    if matches!(policy.action, PolicyAction::RequireApproval) {
        let decided =
            handle_approval_gate(ctx, session_id, meta, &invocation, tool_id, policy.prompt)
                .await?;
        if !decided.allow_execution {
            append_tool_result_event(
                ctx,
                session_id,
                tool_id,
                &invocation,
                &decided.denied_output,
            )
            .await?;
            return Ok(());
        }
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
            workspace_id: meta.workspace_id.clone(),
            user_id: meta.user_id.clone(),
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

async fn handle_dispatch(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    let dispatch_input: DispatchSubAgentInput = serde_json::from_value(invocation.input.clone())
        .map_err(|error| {
            TerminalError::new(format!(
                "failed to deserialize dispatch_sub_agent input: {error}"
            ))
        })?;

    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let span = tool_dispatch_span("dispatch_sub_agent");
    let dispatch_started = Instant::now();
    let dispatched = dispatch_child(ctx, session_id, meta, dispatch_input)
        .instrument(span)
        .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    let output = sub_agent_result_tool_output(&dispatched.result);
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

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = match invocation.name.as_str() {
        "spawn_sub_agent" => {
            let input: SpawnSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize spawn_sub_agent input: {error}"
                    ))
                })?;
            let output = spawn_child_detached(ctx, session_id, meta, input)
                .instrument(span)
                .await?;
            crate::delegation::spawn_output(output)
        }
        "wait_sub_agent" => {
            let input: WaitSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize wait_sub_agent input: {error}"
                    ))
                })?;
            crate::delegation::wait_output(
                wait_child(ctx, session_id, input).instrument(span).await?,
            )
        }
        "message_sub_agent" => {
            let input: MessageSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize message_sub_agent input: {error}"
                    ))
                })?;
            let sub_agent_id = input.sub_agent_id.clone();
            message_child(ctx, session_id, input)
                .instrument(span)
                .await?;
            crate::delegation::message_output(&sub_agent_id)
        }
        "list_sub_agents" => {
            let input: ListSubAgentsInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize list_sub_agents input: {error}"
                    ))
                })?;
            crate::delegation::list_output(
                list_children(ctx, session_id, input)
                    .instrument(span)
                    .await?,
            )
        }
        "cancel_sub_agent" => {
            let input: CancelSubAgentInput = serde_json::from_value(invocation.input.clone())
                .map_err(|error| {
                    TerminalError::new(format!(
                        "failed to deserialize cancel_sub_agent input: {error}"
                    ))
                })?;
            let sub_agent_id = input.sub_agent_id.clone();
            cancel_child(ctx, session_id, input)
                .instrument(span)
                .await?;
            crate::delegation::cancel_output(&sub_agent_id)
        }
        _ => {
            return Err(TerminalError::new(format!(
                "unsupported delegation tool {}",
                invocation.name
            ))
            .into());
        }
    };
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

async fn spawn_child_detached(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    request: SpawnSubAgentInput,
) -> Result<SpawnSubAgentOutput, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    let dispatch_input = DispatchSubAgentInput::from(request.clone());
    let task = dispatch_input.task.clone();
    let budget_tokens = dispatch_input.budget_tokens;
    let hash = validate_dispatch_limits(
        0,
        &children,
        dispatch_input.task.as_str(),
        &dispatch_input.tool_subset,
    )?;
    validate_dispatch_budget(dispatch_input.budget_tokens, None)?;
    let sub_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(uuid::Uuid::now_v7().to_string())) })
        .name("spawn_sub_agent_id")
        .await?
        .into_inner();
    let sub_id = format!("{}-{sub_id}", ctx.key());
    register_session_child(
        ctx,
        session_id,
        SubAgentChildRef {
            id: sub_id.clone(),
            task_hash: hash,
            budget_tokens,
        },
    )
    .await?;

    let initial = dispatch_input.into_initial_message(
        session_id,
        None,
        1,
        String::new(),
        meta.workspace_id.clone(),
        meta.user_id.clone(),
        meta.model.clone(),
    );

    let path = child_agent_path(ctx.key(), &sub_id, request.task_name.as_deref());
    ctx.object_client::<SubAgentClient>(sub_id.clone())
        .post_message(Json::from(initial))
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentSpawned {
            sub_agent_id: sub_id.clone(),
            parent_sub_agent_id: None,
            path: path.clone(),
            task,
            budget_tokens,
        },
    )
    .await?;

    Ok(SpawnSubAgentOutput {
        sub_agent_id: sub_id.clone(),
        path,
        status: moa_core::SubAgentState::Running,
    })
}

async fn wait_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    input: WaitSubAgentInput,
) -> Result<WaitSubAgentOutput, HandlerError> {
    let timeout_ms = crate::delegation::clamp_wait_timeout_ms(input.timeout_ms);
    let mut waited_ms = 0;
    ensure_child_owned_by_session(ctx, session_id, &input.sub_agent_id).await?;
    loop {
        let status = ctx
            .object_client::<SubAgentClient>(input.sub_agent_id.clone())
            .status()
            .call()
            .await?
            .into_inner();
        if crate::delegation::is_terminal_sub_agent_state(status.state) {
            let result = ctx
                .object_client::<SubAgentClient>(input.sub_agent_id.clone())
                .result()
                .call()
                .await?
                .into_inner()
                .ok_or_else(|| TerminalError::new("terminal sub-agent result missing"))?;
            remove_session_child(ctx, session_id, &input.sub_agent_id).await?;
            return Ok(WaitSubAgentOutput {
                sub_agent_id: input.sub_agent_id,
                state: status.state,
                result: Some(result),
                timed_out: false,
            });
        }

        if waited_ms >= timeout_ms {
            return Ok(WaitSubAgentOutput {
                sub_agent_id: input.sub_agent_id,
                state: status.state,
                result: None,
                timed_out: true,
            });
        }

        let sleep_ms = crate::delegation::WAIT_POLL_INTERVAL_MS.min(timeout_ms - waited_ms);
        ctx.sleep(Duration::from_millis(sleep_ms)).await?;
        waited_ms += sleep_ms;
    }
}

async fn message_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    input: MessageSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned_by_session(ctx, session_id, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(sub_agent_id.clone())
        .post_message(Json::from(moa_core::SubAgentMessage::FollowUp {
            text: input.text.clone(),
        }))
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentMessageSent {
            sub_agent_id,
            parent_sub_agent_id: None,
            text: input.text,
        },
    )
    .await?;
    Ok(())
}

async fn list_children(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    _input: ListSubAgentsInput,
) -> Result<ListSubAgentsOutput, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    let mut sub_agents = Vec::with_capacity(children.len());
    for child in children {
        let status = ctx
            .object_client::<SubAgentClient>(child.id.clone())
            .status()
            .call()
            .await?
            .into_inner();
        sub_agents.push(crate::delegation::listed_sub_agent(child.id, status));
    }
    Ok(ListSubAgentsOutput { sub_agents })
}

async fn cancel_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    input: CancelSubAgentInput,
) -> Result<(), HandlerError> {
    ensure_child_owned_by_session(ctx, session_id, &input.sub_agent_id).await?;
    let sub_agent_id = input.sub_agent_id.clone();
    ctx.object_client::<SubAgentClient>(input.sub_agent_id)
        .cancel(input.reason)
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentStatusChanged {
            sub_agent_id,
            from: None,
            to: moa_core::SubAgentState::Cancelled,
            summary: Some("cancel requested by parent".to_string()),
        },
    )
    .await?;
    Ok(())
}

async fn ensure_child_owned_by_session(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    sub_agent_id: &str,
) -> Result<(), HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    if child_is_owned(&children, sub_agent_id) {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "sub-agent {sub_agent_id} is not owned by this parent"
    ))
    .into())
}

async fn session_child_refs(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<SubAgentChildRef>, HandlerError> {
    Ok(ctx
        .object_client::<SessionClient>(session_id.to_string())
        .child_refs()
        .call()
        .await?
        .into_inner())
}

async fn register_session_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    child: SubAgentChildRef,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .register_child(Json::from(child))
        .call()
        .await?;
    Ok(())
}

async fn remove_session_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    sub_agent_id: &str,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .remove_child(sub_agent_id.to_string())
        .call()
        .await?;
    Ok(())
}

async fn dispatch_child(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    request: DispatchSubAgentInput,
) -> Result<DispatchedSubAgent, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    let hash = validate_dispatch_limits(0, &children, request.task.as_str(), &request.tool_subset)?;
    validate_dispatch_budget(request.budget_tokens, None)?;
    let task = request.task.clone();
    let budget_tokens = request.budget_tokens;
    let sub_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(uuid::Uuid::now_v7().to_string())) })
        .name("dispatch_sub_agent_id")
        .await?
        .into_inner();
    let sub_id = format!("{}-{sub_id}", ctx.key());
    register_session_child(
        ctx,
        session_id,
        SubAgentChildRef {
            id: sub_id.clone(),
            task_hash: hash,
            budget_tokens,
        },
    )
    .await?;

    let (awakeable_id, result_future) = ctx.awakeable::<String>();
    let initial = request.into_initial_message(
        session_id,
        None,
        1,
        awakeable_id,
        meta.workspace_id.clone(),
        meta.user_id.clone(),
        meta.model.clone(),
    );

    ctx.object_client::<SubAgentClient>(sub_id.clone())
        .post_message(Json::from(initial))
        .send();
    append_session_event(
        ctx,
        session_id,
        Event::SubAgentSpawned {
            sub_agent_id: sub_id.clone(),
            parent_sub_agent_id: None,
            path: child_agent_path(ctx.key(), &sub_id, None),
            task,
            budget_tokens,
        },
    )
    .await?;

    let result: moa_core::SubAgentResult =
        serde_json::from_str(&result_future.await?).map_err(|error| {
            TerminalError::new(format!(
                "failed to deserialize sub-agent result from awakeable: {error}"
            ))
        })?;

    remove_session_child(ctx, session_id, &sub_id).await?;

    Ok(DispatchedSubAgent { id: sub_id, result })
}

struct ApprovalOutcome {
    allow_execution: bool,
    denied_output: ToolOutput,
}

impl ApprovalOutcome {
    fn allow_execution() -> Self {
        Self {
            allow_execution: true,
            denied_output: ToolOutput::error("", Duration::ZERO),
        }
    }

    fn deny(denied_output: ToolOutput) -> Self {
        Self {
            allow_execution: false,
            denied_output,
        }
    }
}

async fn handle_approval_gate(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    invocation: &ToolInvocation,
    tool_id: ToolCallId,
    prompt: Option<ApprovalPrompt>,
) -> Result<ApprovalOutcome, HandlerError> {
    let mut prompt = prompt.ok_or_else(|| {
        TerminalError::new(format!(
            "workspace store did not return an approval prompt for tool {}",
            invocation.name
        ))
    })?;
    let (awakeable_id, awakeable) = ctx.awakeable::<String>();
    let pending = PendingApprovalState {
        awakeable_id: awakeable_id.clone(),
        request_id: tool_id.0,
        session_id,
        sub_agent_id: None,
    };
    ctx.set(K_PENDING_APPROVAL, Json::from(pending));
    prompt.request.sub_agent_id = None;

    append_session_event(
        ctx,
        session_id,
        Event::ApprovalRequested {
            request_id: prompt.request.request_id,
            awakeable_id: Some(awakeable_id),
            sub_agent_id: None,
            tool_name: prompt.request.tool_name.clone(),
            input_summary: prompt.request.input_summary.clone(),
            risk_level: prompt.request.risk_level.clone(),
            prompt: prompt.clone(),
        },
    )
    .await?;

    score_current_active_segment(ctx, session_id, ScoringPhase::Immediate, &[]).await?;
    update_session_status(ctx, session_id, SessionStatus::WaitingApproval).await?;
    let approval_timeout = approval_wait_timeout();
    let timed_out_reason = approval_wait::timeout_reason(approval_timeout);
    let approval_started = Instant::now();
    let decision = restate_sdk::select! {
        decision = awakeable => {
            parse_awakeable_decision(&decision?)?
        },
        reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
            ApprovalDecision::Deny {
                reason: Some(approval_wait::cancel_reason(&reason?)),
            }
        },
        _ = ctx.sleep(approval_timeout) => {
            ApprovalDecision::Deny {
                reason: Some(timed_out_reason.clone()),
            }
        }
    };
    record_approval_wait(
        approval_started.elapsed(),
        approval_wait::outcome_label(&decision, &timed_out_reason),
    );
    ctx.clear(K_PENDING_APPROVAL);
    update_session_status(ctx, session_id, SessionStatus::Running).await?;

    let decided_by = approval_wait::system_decider_for(&decision, &timed_out_reason)
        .unwrap_or_else(|| meta.user_id.as_str())
        .to_string();

    append_session_event(
        ctx,
        session_id,
        Event::ApprovalDecided {
            request_id: prompt.request.request_id,
            sub_agent_id: None,
            decision: decision.clone(),
            decided_by,
            decided_at: durable_utc_now(ctx).await?,
        },
    )
    .await?;

    match decision {
        ApprovalDecision::AllowOnce => Ok(ApprovalOutcome::allow_execution()),
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
            Ok(ApprovalOutcome::allow_execution())
        }
        ApprovalDecision::Deny { reason } => {
            let message = reason.unwrap_or_else(|| "Denied by the user".to_string());
            Ok(ApprovalOutcome::deny(denied_tool_output(format!(
                "Tool execution denied: {message}"
            ))))
        }
    }
}

async fn cleanup_pending_approval_after_cancel(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    reason: &str,
) -> Result<(), HandlerError> {
    let Some(pending) = ctx
        .get::<Json<PendingApprovalState>>(K_PENDING_APPROVAL)
        .await?
        .map(Json::into_inner)
    else {
        return Ok(());
    };

    let decision = ApprovalDecision::Deny {
        reason: Some(approval_wait::cancel_reason(reason)),
    };
    let serialized = serialize_awakeable_decision(&decision)?;
    ctx.resolve_awakeable(&pending.awakeable_id, serialized);
    ctx.clear(K_PENDING_APPROVAL);
    append_session_event(
        ctx,
        pending.session_id,
        Event::ApprovalDecided {
            request_id: pending.request_id,
            sub_agent_id: pending.sub_agent_id,
            decision,
            decided_by: "system:cancel".to_string(),
            decided_at: durable_utc_now(ctx).await?,
        },
    )
    .await?;
    update_session_status(ctx, session_id, SessionStatus::Cancelled).await?;
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
        meta.workspace_id.as_str(),
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
            score_completed_segment_at_transition(
                ctx,
                session_id,
                meta.workspace_id.as_str(),
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

async fn score_completed_segment_at_transition(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tenant_id: &str,
    completed: &moa_brain::pipeline::segments::SegmentCompleted,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    if !OrchestratorCtx::current().config.resolution.enabled {
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
        let events = load_segment_scoring_events(
            ctx,
            session_id,
            completed.segment_id,
            boundary,
            next_user.1,
            true,
        )
        .await?;
        (
            segment_events_for_scoring(&events, completed.segment_id, next_user.1),
            next_user.0,
        )
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %completed.segment_id,
            "segment start event missing; falling back to full event log for completed segment scoring"
        );
        let events = load_session_events_fallback(ctx, session_id).await?;
        let (next_user_message, next_user_seq) = latest_user_message(&events)
            .map(|(text, sequence_num)| (Some(text.to_string()), Some(sequence_num)))
            .unwrap_or((None, None));
        let segment_events =
            segment_events_for_scoring(&events, completed.segment_id, next_user_seq);
        (segment_events, next_user_message)
    };
    let rewrite = query_rewrite_from_metadata(metadata);
    let baseline = load_segment_baseline(ctx, tenant_id).await?;
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

async fn score_current_active_segment(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    phase: ScoringPhase,
    overrides: &[ResolutionOverride],
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config.resolution.enabled {
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
            load_segment_scoring_events(ctx, session_id, segment.id, boundary, None, true).await?;
        segment_events_for_scoring(&events, segment.id, None)
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %segment.id,
            "segment start event missing; falling back to full event log for active segment scoring"
        );
        let events = load_session_events_fallback(ctx, session_id).await?;
        segment_events_for_scoring(&events, segment.id, None)
    };
    let baseline = load_segment_baseline(ctx, meta.workspace_id.as_str()).await?;
    let duration_ms = durable_utc_now(ctx)
        .await?
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

    record_resolution_learning(ctx, meta.workspace_id.as_str(), segment.id, &score).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_resolution_score(Json(UpdateSegmentResolutionScoreRequest {
            segment_id: segment.id,
            score,
        }))
        .send();
    Ok(())
}

async fn record_resolution_learning(
    ctx: &WorkflowContext<'_>,
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

async fn load_events_in_range(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    range: EventRange,
    operation_name: &'static str,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current().session_store.clone();
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

async fn load_segment_scoring_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    segment_id: SegmentId,
    boundary: SegmentBoundarySequences,
    cutoff_before_seq: Option<u64>,
    stop_at_completion: bool,
) -> Result<Vec<EventRecord>, HandlerError> {
    let to_seq = segment_scoring_to_seq(boundary, cutoff_before_seq, stop_at_completion);
    tracing::debug!(
        session_id = %session_id,
        segment_id = %segment_id,
        from_seq = boundary.start_seq,
        to_seq = ?to_seq,
        "loading bounded events for segment scoring"
    );
    load_events_in_range(
        ctx,
        session_id,
        EventRange {
            from_seq: Some(boundary.start_seq),
            to_seq,
            ..EventRange::default()
        },
        "turn_execution_load_segment_scoring_events",
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
            "current user message sequence was not found during completed segment scoring"
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
    let store = OrchestratorCtx::current().session_store.clone();
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

fn segment_scoring_to_seq(
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
    tenant_id: &str,
) -> Result<Option<moa_core::SegmentBaseline>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_segment_baseline(Json(GetSegmentBaselineRequest {
            tenant_id: tenant_id.to_string(),
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
    let store = OrchestratorCtx::current().session_store.clone();
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

async fn update_session_status(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    status: SessionStatus,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .update_status(Json(UpdateStatusRequest { session_id, status }))
        .call()
        .await
        .map_err(HandlerError::from)
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
        OrchestratorCtx::current()
            .config
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

fn approval_wait_timeout() -> Duration {
    approval_wait::timeout_from_env(
        std::env::var(APPROVAL_TIMEOUT_SECS_ENV).ok().as_deref(),
        DEFAULT_APPROVAL_TIMEOUT_SECS,
    )
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

fn notify_session_of_outcome(ctx: &WorkflowContext<'_>, session_id: &str, outcome: &TurnOutcome) {
    use crate::objects::session::SessionClient;

    ctx.object_client::<SessionClient>(session_id.to_string())
        .record_turn_outcome(Json::from(outcome.clone()))
        .send();
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{Event, EventRecord, SegmentId, SessionId};
    use uuid::Uuid;

    use super::{
        SegmentBoundarySequences, segment_boundary_sequences, segment_events_for_scoring,
        segment_scoring_to_seq,
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
    fn segment_scoring_to_seq_prefers_next_user_cutoff() {
        // Pins: completed segment scoring ends before the next user message when known.
        let boundary = SegmentBoundarySequences {
            start_seq: 10,
            completed_seq: Some(40),
        };

        assert_eq!(segment_scoring_to_seq(boundary, Some(35), true), Some(34));
        assert_eq!(segment_scoring_to_seq(boundary, None, true), Some(40));
        assert_eq!(segment_scoring_to_seq(boundary, None, false), None);
    }

    #[test]
    fn segment_events_for_scoring_starts_at_segment_and_stops_before_cutoff() {
        // Pins: segment scoring excludes prior events and the next task's user message.
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

        let filtered = segment_events_for_scoring(&events, target_segment, Some(5));
        let sequences = filtered
            .iter()
            .map(|record| record.sequence_num)
            .collect::<Vec<_>>();

        assert_eq!(sequences, vec![2, 3, 4]);
    }
}
