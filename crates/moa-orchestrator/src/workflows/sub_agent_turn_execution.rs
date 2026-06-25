//! Workflow-backed execution for one sub-agent turn run.
//!
//! The `SubAgent` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use moa_core::wire::{
    AppendEventRequest, RecordSegmentToolUseRequest, RunSubAgentTurnRequest, TurnComplexityClass,
    TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{
    ActionPolicyEffect, CompletionContent, CompletionRequest, CompletionResponse, Event, ModelTier,
    SessionId, SessionMeta, StopReason, SubAgentToolRecord, SubAgentTurnOutcomeRecord,
    SubAgentTurnPreparation, SubAgentTurnResponseRecord, TokenUsage, ToolCallContent, ToolCallId,
    ToolCallRequest, ToolInvocation, ToolOutput, TurnOutcome as CoreTurnOutcome,
    is_delegation_tool_name,
};
use moa_observability::restate_observability::{
    annotate_restate_handler_span, event_persist_span, llm_call_span, sub_agent_turn_span,
    tool_dispatch_span,
};
use moa_observability::{
    record_session_error, record_turn_event_persist_duration, record_turn_llm_call_duration,
    record_turn_tool_dispatch_duration, record_turn_workflow_outcome,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::objects::sub_agent::{MAX_SUB_AGENT_TURNS_PER_WORKFLOW, SubAgentClient};
use crate::services::{
    action_reviews::{ActionReviewsClient, RequestActionReview},
    llm_gateway::LLMGatewayClient,
    session_store::RestateSessionStoreClient,
    tool_executor::ToolExecutorClient,
    workspace_store::{PrepareActionReviewRequest, WorkspaceStoreClient},
};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification, blocked_canary_tool_output,
    denied_tool_output, disallowed_tool_output, meaningful_cancel_reason, response_tool_calls,
    stable_tool_call_id, tool_call_is_allowed, tool_input_leaks_canary, turn_outcome_for_response,
};
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL};
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState, TurnResponsivenessInput,
    classify_turn_request, effective_tool_cap, effective_turn_cap, progress_cap, progress_count,
};

const K_CANCEL_REASON_PROMISE: &str = "cancel_reason";
const K_PHASE: &str = "phase";
const K_COMPLEXITY_CLASS: &str = "complexity_class";
const K_ITERATION: &str = "iteration";
const K_MAX_TURNS: &str = "max_turns";
const K_TOOL_CALLS: &str = "tool_calls";
const K_MAX_TOOL_CALLS: &str = "max_tool_calls";

#[derive(Clone, Debug)]
enum SubAgentIterationOutcome {
    Core(CoreTurnOutcome),
    Cancelled(String),
    ToolBudgetExceeded(String),
}

struct SubAgentIterationInput<'a> {
    request: &'a RunSubAgentTurnRequest,
    completion_request: CompletionRequest,
    active_canary: Option<String>,
    meta: SessionMeta,
    parent_session: SessionId,
    turn_evidence: &'a mut TurnEvidence,
    tool_budget: &'a mut ToolBudgetState,
}

/// Restate workflow surface for durable sub-agent turn execution.
#[restate_sdk::workflow]
pub trait SubAgentTurnExecution {
    /// Runs one sub-agent turn workflow body.
    async fn run(request: Json<RunSubAgentTurnRequest>) -> Result<Json<TurnOutcome>, HandlerError>;

    /// Requests cancellation of the in-flight sub-agent turn workflow.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Returns workflow progress without blocking the workflow body.
    #[shared]
    async fn progress() -> Result<Json<TurnProgress>, HandlerError>;
}

/// Concrete `SubAgentTurnExecution` workflow implementation.
pub struct SubAgentTurnExecutionImpl;

impl SubAgentTurnExecution for SubAgentTurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunSubAgentTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "run");
        let request = request.into_inner();
        ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));

        let workflow_started = Instant::now();
        let outcome = match run_sub_agent_inside_workflow(&ctx, &request).await {
            Ok(outcome) => outcome,
            Err(error) => TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Failed,
                message: format!("{error:?}"),
            },
        };
        record_turn_workflow_outcome(
            "sub_agent",
            turn_outcome_kind_label(&outcome.kind),
            ModelTier::Auxiliary,
            workflow_started.elapsed(),
        );
        let phase = match outcome.kind {
            TurnOutcomeKind::Completed => TurnPhase::Completed,
            TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
            TurnOutcomeKind::Failed => TurnPhase::Failed,
        };
        turn_progress::finish(&ctx).await?;
        ctx.set(K_PHASE, Json::from(phase));
        notify_sub_agent_of_outcome(&ctx, &request.sub_agent_id, &outcome);
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "request_cancel");
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
        annotate_restate_handler_span("SubAgentTurnExecution", "progress");
        let phase = ctx
            .get::<Json<TurnPhase>>(K_PHASE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let cancel_reason =
            meaningful_cancel_reason(ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?);
        let complexity_class = ctx
            .get::<Json<TurnComplexityClass>>(K_COMPLEXITY_CLASS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let iteration = ctx
            .get::<Json<u32>>(K_ITERATION)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let max_turns = ctx
            .get::<Json<Option<u32>>>(K_MAX_TURNS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(None);
        let tool_calls = ctx
            .get::<Json<u32>>(K_TOOL_CALLS)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let max_tool_calls = ctx
            .get::<Json<Option<u32>>>(K_MAX_TOOL_CALLS)
            .await?
            .map(Json::into_inner)
            .unwrap_or(None);
        let progress = turn_progress::snapshot(&ctx, is_terminal_phase(&phase)).await?;
        Ok(Json::from(TurnProgress {
            turn_id: ctx.key().to_string(),
            phase,
            complexity_class,
            iteration,
            max_turns,
            tool_calls,
            max_tool_calls,
            elapsed_ms: progress.elapsed_ms,
            last_progress_summary: progress.last_summary,
            cancel_requested: cancel_reason.is_some(),
            cancel_reason,
        }))
    }
}

async fn run_sub_agent_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
) -> Result<TurnOutcome, HandlerError> {
    let session_limits = &OrchestratorCtx::current_config().session_limits;
    let selected_class = classify_turn_request(TurnResponsivenessInput {
        user_text: "",
        attachment_count: 0,
        request_max_turns: request.max_turns,
        has_recent_target: true,
        is_workflow_context: false,
        is_sub_agent_context: true,
        available_tool_count: 0,
    });
    let request_or_default_cap = request.max_turns.or(Some(
        MAX_SUB_AGENT_TURNS_PER_WORKFLOW.min(u32::MAX as usize) as u32,
    ));
    let max_turns = effective_turn_cap(request_or_default_cap, selected_class, session_limits);
    let max_tool_calls = effective_tool_cap(selected_class, session_limits);
    let mut tool_budget =
        ToolBudgetState::new(max_tool_calls, session_limits.loop_detection_threshold);
    ctx.set(K_COMPLEXITY_CLASS, Json::from(selected_class));
    ctx.set(K_ITERATION, Json::from(0_u32));
    ctx.set(K_MAX_TURNS, Json::from(progress_cap(max_turns)));
    ctx.set(K_TOOL_CALLS, Json::from(0_u32));
    ctx.set(K_MAX_TOOL_CALLS, Json::from(progress_cap(max_tool_calls)));
    turn_progress::initialize(ctx).await?;
    let mut turn_evidence = TurnEvidence::default();
    let mut last_request_meta = None;
    let mut last_parent_session = None;
    for turn_number in 1..=max_turns {
        ctx.set(K_ITERATION, Json::from(progress_count(turn_number)));
        if let Some(reason) = cancel_requested(ctx).await? {
            ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        ctx.set(K_PHASE, Json::from(TurnPhase::Compiling));
        let preparation = ctx
            .object_client::<SubAgentClient>(request.sub_agent_id.clone())
            .prepare_turn()
            .call()
            .await?
            .into_inner();
        let (completion_request, active_canary, meta, parent_session) = match preparation {
            SubAgentTurnPreparation::Outcome { outcome } => {
                return Ok(workflow_outcome_from_core(request, outcome));
            }
            SubAgentTurnPreparation::Request {
                request,
                active_canary,
                session_meta,
                parent_session,
            } => {
                last_request_meta = Some((*session_meta).clone());
                last_parent_session = Some(parent_session);
                (*request, active_canary, *session_meta, parent_session)
            }
        };
        let turn_span = sub_agent_turn_span(
            &meta,
            &request.sub_agent_id,
            &request.turn_id,
            turn_number as i64,
            None,
        );
        let outcome = run_sub_agent_iteration(
            ctx,
            SubAgentIterationInput {
                request,
                completion_request,
                active_canary,
                meta,
                parent_session,
                turn_evidence: &mut turn_evidence,
                tool_budget: &mut tool_budget,
            },
        )
        .instrument(turn_span)
        .await?;
        match outcome {
            SubAgentIterationOutcome::Cancelled(message) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message,
                });
            }
            SubAgentIterationOutcome::ToolBudgetExceeded(message) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message,
                });
            }
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Idle) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message: "sub-agent turn completed".to_string(),
                });
            }
            SubAgentIterationOutcome::Core(CoreTurnOutcome::Cancelled) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message: "sub-agent turn cancelled".to_string(),
                });
            }
        }
    }

    if let (Some(meta), Some(parent_session)) = (last_request_meta.as_ref(), last_parent_session) {
        let message =
            record_sub_agent_turn_budget_stop(ctx, request, meta, parent_session, max_turns)
                .await?;
        return Ok(TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message,
        });
    }

    Ok(TurnOutcome {
        turn_id: request.turn_id.clone(),
        kind: TurnOutcomeKind::Failed,
        message: format!("sub-agent turn budget exceeded ({max_turns})"),
    })
}

async fn run_sub_agent_iteration(
    ctx: &WorkflowContext<'_>,
    mut input: SubAgentIterationInput<'_>,
) -> Result<SubAgentIterationOutcome, HandlerError> {
    attach_active_segment_metadata(ctx, input.parent_session, &mut input.completion_request)
        .await?;
    let allowed_tools = allowed_tool_names(&input.completion_request);

    ctx.set(K_PHASE, Json::from(TurnPhase::Streaming));
    let (progress_first_delay_ms, progress_interval_ms) = progress_cadence();
    turn_progress::maybe_emit(
        ctx,
        input.parent_session,
        &input.request.turn_id,
        TurnPhase::Streaming,
        SUMMARY_CALLING_MODEL,
        progress_first_delay_ms,
        progress_interval_ms,
    )
    .await?;
    let span = llm_call_span(&input.meta);
    let llm_started = Instant::now();
    let response = {
        let _guard = span.enter();
        restate_sdk::select! {
            reason = ctx.promise::<String>(K_CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                ctx.object_client::<SubAgentClient>(input.request.sub_agent_id.clone())
                    .cancel(reason.clone())
                    .send();
                return Ok(SubAgentIterationOutcome::Cancelled(reason));
            },
            response = ctx
                .service_client::<LLMGatewayClient>()
                .complete(Json::from(input.completion_request))
                .call() => {
                    response?.into_inner()
                }
        }
    };
    record_turn_llm_call_duration(llm_started.elapsed());
    let (response, verification_annotated) =
        annotate_unresolved_verification(&response, &*input.turn_evidence);

    ctx.object_client::<SubAgentClient>(input.request.sub_agent_id.clone())
        .record_response(Json::from(SubAgentTurnResponseRecord {
            turn_id: input.request.turn_id.clone(),
            response: response.clone(),
        }))
        .call()
        .await?;

    if verification_annotated {
        let outcome = CoreTurnOutcome::Idle;
        ctx.object_client::<SubAgentClient>(input.request.sub_agent_id.clone())
            .apply_turn_outcome(Json::from(SubAgentTurnOutcomeRecord {
                turn_id: input.request.turn_id.clone(),
                outcome,
            }))
            .call()
            .await?;
        return Ok(SubAgentIterationOutcome::Core(outcome));
    }

    for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
        if let Some(reason) = cancel_requested(ctx).await? {
            ctx.object_client::<SubAgentClient>(input.request.sub_agent_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(SubAgentIterationOutcome::Cancelled(reason));
        }
        match input
            .tool_budget
            .before_tool_dispatch(&tool_call.invocation)
        {
            ToolBudgetDecision::Allow {
                attempted_tool_calls,
            } => ctx.set(
                K_TOOL_CALLS,
                Json::from(progress_count(attempted_tool_calls)),
            ),
            ToolBudgetDecision::Stop(exhaustion) => {
                ctx.set(
                    K_TOOL_CALLS,
                    Json::from(progress_count(input.tool_budget.attempted_tool_calls())),
                );
                let message = record_sub_agent_budget_stop(
                    ctx,
                    input.request,
                    &input.meta,
                    input.parent_session,
                    &exhaustion,
                )
                .await?;
                return Ok(SubAgentIterationOutcome::ToolBudgetExceeded(message));
            }
        }
        let tool_context = SubAgentToolContext {
            turn_id: &input.request.turn_id,
            sub_agent_id: &input.request.sub_agent_id,
            meta: &input.meta,
            session_id: input.parent_session,
            active_canary: input.active_canary.as_deref(),
        };
        handle_tool_call(
            ctx,
            tool_context,
            &allowed_tools,
            index,
            tool_call,
            &mut *input.turn_evidence,
        )
        .await?;
    }

    let outcome = turn_outcome_for_response(&response);
    ctx.object_client::<SubAgentClient>(input.request.sub_agent_id.clone())
        .apply_turn_outcome(Json::from(SubAgentTurnOutcomeRecord {
            turn_id: input.request.turn_id.clone(),
            outcome,
        }))
        .call()
        .await?;
    Ok(SubAgentIterationOutcome::Core(outcome))
}

async fn attach_active_segment_metadata(
    ctx: &WorkflowContext<'_>,
    parent_session: SessionId,
    request: &mut CompletionRequest,
) -> Result<(), HandlerError> {
    let Some(segment) = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(parent_session))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view())
    else {
        return Ok(());
    };
    request.metadata.insert(
        "_moa.segment_id".to_string(),
        serde_json::json!(segment.id.to_string()),
    );
    request.metadata.insert(
        "_moa.segment_index".to_string(),
        serde_json::json!(segment.segment_index),
    );
    Ok(())
}

struct SubAgentToolContext<'a> {
    turn_id: &'a str,
    sub_agent_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    active_canary: Option<&'a str>,
}

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    tool_context: SubAgentToolContext<'_>,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    ctx.set(K_PHASE, Json::from(TurnPhase::Tooling));
    let sub_agent_id = tool_context.sub_agent_id;
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let invocation = tool_call.invocation.clone();

    if !tool_call_is_allowed(allowed_tools, &invocation.name) {
        append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
        let output = disallowed_tool_output(&invocation.name);
        append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
        record_denied_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            tool_id,
            &invocation,
            &output,
        )
        .await?;
        turn_evidence.record_tool_result(&invocation, &output);
        return Ok(());
    }

    if is_delegation_tool_name(&invocation.name) {
        handle_delegation_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            session_id,
            tool_id,
            tool_call,
            turn_evidence,
        )
        .await?;
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
            sub_agent_id: Some(sub_agent_id.to_string()),
            origin_kind: Some("sub_agent".to_string()),
            origin_id: Some(sub_agent_id.to_string()),
            origin_step_id: Some(tool_context.turn_id.to_string()),
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
        record_denied_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            tool_id,
            &invocation,
            &output,
        )
        .await?;
        turn_evidence.record_tool_result(&invocation, &output);
        return Ok(());
    }

    if matches!(prepared_action.effect, ActionPolicyEffect::AdminReview) {
        let tool_request = ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: tool_context.active_canary.map(ToOwned::to_owned),
            session_id: Some(session_id),
            tenant_id: meta.tenant_id,
            user_id: storage_user_id(meta),
            idempotency_key: invocation.id.clone(),
        };
        if tool_input_leaks_canary(tool_context.active_canary, &tool_request.input)
            .map_err(|error| TerminalError::new(format!("serialize tool input: {error}")))?
        {
            let output = blocked_canary_tool_output(&invocation.name);
            append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
            record_denied_tool(
                ctx,
                tool_context.turn_id,
                sub_agent_id,
                tool_id,
                &invocation,
                &output,
            )
            .await?;
            turn_evidence.record_tool_result(&invocation, &output);
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
        record_denied_tool(
            ctx,
            tool_context.turn_id,
            sub_agent_id,
            tool_id,
            &invocation,
            &output,
        )
        .await?;
        turn_evidence.record_tool_result(&invocation, &output);
        return Ok(());
    }

    let span = tool_dispatch_span(&invocation.name);
    let (progress_first_delay_ms, progress_interval_ms) = progress_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        tool_context.turn_id,
        TurnPhase::Tooling,
        turn_progress::running_tool_summary(&invocation.name),
        progress_first_delay_ms,
        progress_interval_ms,
    )
    .await?;
    let dispatch_started = Instant::now();
    let output = ctx
        .service_client::<ToolExecutorClient>()
        .execute(Json::from(ToolCallRequest {
            tool_call_id: tool_id,
            provider_tool_use_id: invocation.id.clone(),
            tool_name: invocation.name.clone(),
            input: invocation.input.clone(),
            active_canary: tool_context.active_canary.map(ToOwned::to_owned),
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

    record_tool_result(
        ctx,
        tool_context.turn_id,
        sub_agent_id,
        tool_id,
        &invocation,
        &output,
    )
    .await?;
    turn_evidence.record_tool_result(&invocation, &output);
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    parent_sub_agent_id: &str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    let Some(tool) = moa_core::DelegationTool::from_invocation(&invocation)
        .map_err(|error| TerminalError::new(error.to_string()))?
    else {
        return Err(
            TerminalError::new(format!("unsupported delegation tool {}", invocation.name)).into(),
        );
    };

    let span = tool_dispatch_span(&invocation.name);
    let (progress_first_delay_ms, progress_interval_ms) = progress_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_id,
        TurnPhase::Tooling,
        turn_progress::running_tool_summary(&invocation.name),
        progress_first_delay_ms,
        progress_interval_ms,
    )
    .await?;
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::SubAgent {
            sub_agent_id: parent_sub_agent_id,
            session_id,
        },
        tool,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_delegation_tool_result(ctx, session_id, tool_id, &invocation, &output).await?;
    record_tool_result(
        ctx,
        turn_id,
        parent_sub_agent_id,
        tool_id,
        &invocation,
        &output,
    )
    .await?;
    turn_evidence.record_tool_result(&invocation, &output);
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn record_sub_agent_budget_stop(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<String, HandlerError> {
    emit_sub_agent_tool_budget_exceeded(ctx, parent_session, exhaustion).await?;
    let message = exhaustion.assistant_message();
    append_zero_cost_assistant_response(ctx, parent_session, meta, message.clone()).await?;
    let response = CompletionResponse {
        text: message.clone(),
        content: vec![CompletionContent::Text(message.clone())],
        stop_reason: StopReason::EndTurn,
        model: meta.model.clone(),
        usage: TokenUsage::default(),
        duration_ms: 0,
        thought_signature: None,
    };
    ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
        .record_response(Json::from(SubAgentTurnResponseRecord {
            turn_id: request.turn_id.clone(),
            response,
        }))
        .call()
        .await?;
    ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
        .apply_turn_outcome(Json::from(SubAgentTurnOutcomeRecord {
            turn_id: request.turn_id.clone(),
            outcome: CoreTurnOutcome::Idle,
        }))
        .call()
        .await?;
    Ok(message)
}

async fn record_sub_agent_turn_budget_stop(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    max_turns: usize,
) -> Result<String, HandlerError> {
    record_session_error("turn_budget");
    append_session_event(
        ctx,
        parent_session,
        Event::Error {
            message: format!("sub-agent turn budget exceeded ({max_turns}), stopping"),
            recoverable: true,
        },
    )
    .await?;
    let message = format!(
        "MOA stopped because this sub-agent reached the model-loop budget ({max_turns}). Narrow the scope or ask MOA to continue."
    );
    append_zero_cost_assistant_response(ctx, parent_session, meta, message.clone()).await?;
    let response = CompletionResponse {
        text: message.clone(),
        content: vec![CompletionContent::Text(message.clone())],
        stop_reason: StopReason::EndTurn,
        model: meta.model.clone(),
        usage: TokenUsage::default(),
        duration_ms: 0,
        thought_signature: None,
    };
    ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
        .record_response(Json::from(SubAgentTurnResponseRecord {
            turn_id: request.turn_id.clone(),
            response,
        }))
        .call()
        .await?;
    ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
        .apply_turn_outcome(Json::from(SubAgentTurnOutcomeRecord {
            turn_id: request.turn_id.clone(),
            outcome: CoreTurnOutcome::Idle,
        }))
        .call()
        .await?;
    Ok(message)
}

async fn emit_sub_agent_tool_budget_exceeded(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<(), HandlerError> {
    record_session_error("tool_budget");
    append_session_event(
        ctx,
        session_id,
        Event::Error {
            message: exhaustion.audit_message(),
            recoverable: true,
        },
    )
    .await
    .map(|_| ())
}

async fn append_zero_cost_assistant_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        session_id,
        Event::BrainResponse {
            text,
            thought_signature: None,
            model: meta.model.clone(),
            model_tier: ModelTier::Auxiliary,
            input_tokens_uncached: 0,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            cost_cents: 0,
            duration_ms: 0,
        },
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
fn parent_session_from_initial_message(
    message: &moa_core::SubAgentMessage,
) -> Result<SessionId, HandlerError> {
    match message {
        moa_core::SubAgentMessage::InitialTask { parent_session, .. } => Ok(*parent_session),
        moa_core::SubAgentMessage::FollowUp { .. } => {
            Err(TerminalError::new("reserved child did not include an initial task message").into())
        }
    }
}

async fn record_tool_result(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_tool_result(Json::from(SubAgentToolRecord {
            turn_id: Some(turn_id.to_string()),
            tool_id,
            invocation: invocation.clone(),
            output: output.clone(),
        }))
        .call()
        .await?;
    Ok(())
}

async fn record_denied_tool(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    sub_agent_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_denied_tool(Json::from(SubAgentToolRecord {
            turn_id: Some(turn_id.to_string()),
            tool_id,
            invocation: invocation.clone(),
            output: output.clone(),
        }))
        .call()
        .await?;
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

async fn append_delegation_tool_result(
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
            success: !output.is_error,
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

async fn cancel_requested(ctx: &WorkflowContext<'_>) -> Result<Option<String>, HandlerError> {
    Ok(meaningful_cancel_reason(
        ctx.peek_promise::<String>(K_CANCEL_REASON_PROMISE).await?,
    ))
}

fn notify_sub_agent_of_outcome(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
    outcome: &TurnOutcome,
) {
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_turn_outcome(Json::from(outcome.clone()))
        .send();
}

fn workflow_outcome_from_core(
    request: &RunSubAgentTurnRequest,
    outcome: CoreTurnOutcome,
) -> TurnOutcome {
    match outcome {
        CoreTurnOutcome::Continue | CoreTurnOutcome::Idle => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message: match outcome {
                CoreTurnOutcome::Continue => "sub-agent turn yielded continuation".to_string(),
                CoreTurnOutcome::Idle => "sub-agent turn completed".to_string(),
                CoreTurnOutcome::Cancelled => unreachable!(),
            },
        },
        CoreTurnOutcome::Cancelled => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Cancelled,
            message: "sub-agent turn cancelled".to_string(),
        },
    }
}

fn is_terminal_phase(phase: &TurnPhase) -> bool {
    matches!(
        phase,
        TurnPhase::Completed | TurnPhase::Cancelled | TurnPhase::Failed
    )
}

fn progress_cadence() -> (u64, u64) {
    let limits = &OrchestratorCtx::current_config().session_limits;
    (limits.progress_first_delay_ms, limits.progress_interval_ms)
}

fn turn_outcome_kind_label(kind: &TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
    }
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

#[cfg(test)]
mod tests {
    use moa_core::SessionId;
    use moa_core::wire::TurnPhase;

    use super::{is_terminal_phase, parent_session_from_initial_message};

    #[test]
    fn terminal_phase_detection_matches_workflow_lifecycle() {
        // Pins: cancellation requests stop mutating completed sub-agent workflows.
        assert!(!is_terminal_phase(&TurnPhase::Pending));
        assert!(!is_terminal_phase(&TurnPhase::Compiling));
        assert!(!is_terminal_phase(&TurnPhase::Streaming));
        assert!(!is_terminal_phase(&TurnPhase::Tooling));
        assert!(!is_terminal_phase(&TurnPhase::Persisting));
        assert!(is_terminal_phase(&TurnPhase::Completed));
        assert!(is_terminal_phase(&TurnPhase::Cancelled));
        assert!(is_terminal_phase(&TurnPhase::Failed));
    }

    #[test]
    fn reserved_child_parent_session_requires_initial_message() {
        // Pins: nested spawn events derive their root session only from validated initial child messages.
        let session_id = SessionId::new();
        let message = moa_core::SubAgentMessage::InitialTask {
            task: "inspect".to_string(),
            tool_subset: Vec::new(),
            budget_tokens: 100,
            max_turns: Some(2),
            parent_session: session_id,
            parent_sub_agent: Some("parent".to_string()),
            depth: 2,
            workspace_id: moa_core::WorkspaceId::new("workspace"),
            user_id: moa_core::UserId::new("user"),
            model: moa_core::ModelId::new("model"),
        };

        assert_eq!(
            parent_session_from_initial_message(&message)
                .expect("initial task should expose parent session"),
            session_id
        );
        let error = parent_session_from_initial_message(&moa_core::SubAgentMessage::FollowUp {
            text: "continue".to_string(),
        })
        .expect_err("follow-up messages should not be accepted as reservations");
        assert!(format!("{error:?}").contains("initial task message"));
    }
}
