//! Workflow-backed execution for one sub-agent turn run.
//!
//! The `SubAgent` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::BTreeSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_core::wire::session_store::{AppendEventRequest, RecordSegmentToolUseRequest};
use moa_core::wire::turn::{
    RunSubAgentTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{
    AgentSignalId, ChildSignalKind, CompletionContent, CompletionRequest, CompletionResponse,
    Event, ModelTier, ParentResumePolicy, SessionId, SessionMeta, SignalSeverity, StopReason,
    SubAgentSignal, SubAgentToolRecord, SubAgentTurnOutcomeRecord, SubAgentTurnPreparation,
    SubAgentTurnResponseRecord, TokenUsage, ToolCallContent, ToolCallId, ToolInvocation,
    ToolOutput, TrustedSandboxFileManifestRef, TurnOutcome as CoreTurnOutcome,
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
use crate::objects::session::SessionClient;
use crate::objects::sub_agent::{MAX_SUB_AGENT_TURNS_PER_WORKFLOW, SubAgentClient};
use crate::services::{llm_gateway::LLMGatewayClient, session_store::RestateSessionStoreClient};
use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationProgress,
    GovernedInvocationRequest, invoke_governed_tool,
    record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification, response_tool_calls,
    stable_tool_call_id, turn_outcome_for_response,
};
use crate::turn_driver::{
    model_loop as driver_model_loop, progress as driver_progress, segments as driver_segments,
};
use crate::workflows::durable_utc_now;
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL};
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState,
};

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
        driver_progress::set_phase(&ctx, TurnPhase::Compiling);

        let workflow_started = Instant::now();
        // `parent_session` is learned during the loop (best-effort for the FAILED-signal
        // emit below). It stays `None` if the workflow errors before the first turn is
        // prepared; the terminal idle-wake still covers waking an idle parent.
        let mut parent_session: Option<SessionId> = None;
        let outcome = match run_sub_agent_inside_workflow(&ctx, &request, &mut parent_session).await
        {
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
        driver_progress::set_phase(&ctx, phase);
        // Control plane: a FAILED outcome raises a Failed attention signal to the owning
        // coordinator (the only child-originated emit in this increment).
        emit_failed_child_signal_if_needed(&ctx, &request.sub_agent_id, parent_session, &outcome)
            .await?;
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
        driver_progress::request_cancel(&ctx, reason.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        annotate_restate_handler_span("SubAgentTurnExecution", "progress");
        driver_progress::snapshot(&ctx).await
    }
}

async fn run_sub_agent_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunSubAgentTurnRequest,
    parent_session_out: &mut Option<SessionId>,
) -> Result<TurnOutcome, HandlerError> {
    let session_limits = &OrchestratorCtx::current_config().session_limits;
    let loop_plan = driver_model_loop::sub_agent_loop_plan(
        driver_model_loop::SubAgentLoopPlanRequest {
            request_max_turns: request.max_turns,
            default_max_turns: MAX_SUB_AGENT_TURNS_PER_WORKFLOW,
        },
        session_limits,
    );
    let max_turns = loop_plan.max_turns;
    let mut tool_budget = loop_plan.tool_budget();
    driver_progress::initialize_loop_progress(
        ctx,
        loop_plan.complexity_class,
        loop_plan.max_turns,
        loop_plan.max_tool_calls,
    );
    turn_progress::initialize(ctx).await?;
    let mut turn_evidence = TurnEvidence::default();
    let mut last_request_meta = None;
    let mut last_parent_session = None;
    for turn_number in 1..=max_turns {
        driver_progress::set_iteration(ctx, turn_number);
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            ctx.object_client::<SubAgentClient>(request.sub_agent_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        driver_progress::set_phase(ctx, TurnPhase::Compiling);
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
                // Surface the owning coordinator session to the caller for the
                // FAILED-signal emit, even if a later turn errors.
                *parent_session_out = Some(parent_session);
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

    driver_progress::set_phase(ctx, TurnPhase::Streaming);
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        input.parent_session,
        &input.request.turn_id,
        TurnPhase::Streaming,
        SUMMARY_CALLING_MODEL,
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;
    record_sub_agent_heartbeat(ctx, &input.request.sub_agent_id).await?;
    let span = llm_call_span(&input.meta);
    let llm_started = Instant::now();
    let response = {
        let _guard = span.enter();
        restate_sdk::select! {
            reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
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
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
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
            } => driver_progress::set_tool_calls(ctx, attempted_tool_calls),
            ToolBudgetDecision::Stop(exhaustion) => {
                driver_progress::set_tool_calls(ctx, input.tool_budget.attempted_tool_calls());
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
            trusted_sandbox_manifest: input.request.trusted_sandbox_manifest.as_ref(),
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

/// Refreshes the child's telemetry-plane heartbeat at the progress cadence.
///
/// The timestamp is journaled via `durable_utc_now` so it stays replay-stable, then
/// fire-and-forget delivered to the `SubAgent` VO. This is VO state only (no event
/// per tick); the watchdog and `progress_summary` read it to detect a stuck child.
async fn record_sub_agent_heartbeat(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx, "sub_agent_heartbeat").await?;
    ctx.object_client::<SubAgentClient>(sub_agent_id.to_string())
        .record_heartbeat(Json::from(now))
        .send();
    Ok(())
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
    driver_segments::insert_active_segment_metadata(request, &segment);
    Ok(())
}

struct SubAgentToolContext<'a> {
    turn_id: &'a str,
    sub_agent_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    active_canary: Option<&'a str>,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
}

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    tool_context: SubAgentToolContext<'_>,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let sub_agent_id = tool_context.sub_agent_id;
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);
    let cadence = driver_progress::current_cadence();
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: meta,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            active_canary: tool_context.active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::SubAgent {
                sub_agent_id,
                turn_id: tool_context.turn_id,
            },
            progress: GovernedInvocationProgress {
                turn_id: tool_context.turn_id,
                first_delay_ms: cadence.first_delay_ms,
                interval_ms: cadence.interval_ms,
            },
        },
    )
    .await?;

    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            if result.should_record_denied_sub_agent_tool() {
                record_denied_tool(
                    ctx,
                    tool_context.turn_id,
                    sub_agent_id,
                    result.tool_id,
                    &result.invocation,
                    &result.output,
                )
                .await?;
            } else {
                record_tool_result(
                    ctx,
                    tool_context.turn_id,
                    sub_agent_id,
                    result.tool_id,
                    &result.invocation,
                    &result.output,
                )
                .await?;
            }
            turn_evidence.record_tool_result(&result.invocation, &result.output);
            if result.should_record_segment_tool_use() {
                record_governed_segment_tool_use(ctx, session_id, &result.invocation.name).await?;
            }
        }
        GovernedInvocationOutcome::Delegation { tool_id, .. } => {
            handle_delegation_tool(
                ctx,
                SubAgentDelegationToolRequest {
                    turn_id: tool_context.turn_id,
                    parent_sub_agent_id: sub_agent_id,
                    session_id,
                    tool_id,
                    tool_call,
                    trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
                },
                turn_evidence,
            )
            .await?;
        }
    }
    Ok(())
}

struct SubAgentDelegationToolRequest<'a> {
    turn_id: &'a str,
    parent_sub_agent_id: &'a str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &'a ToolCallContent,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    request: SubAgentDelegationToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let SubAgentDelegationToolRequest {
        turn_id,
        parent_sub_agent_id,
        session_id,
        tool_id,
        tool_call,
        trusted_sandbox_manifest,
    } = request;
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
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_id,
        TurnPhase::Tooling,
        turn_progress::running_tool_summary(&invocation.name),
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;
    record_sub_agent_heartbeat(ctx, parent_sub_agent_id).await?;
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::SubAgent {
            sub_agent_id: parent_sub_agent_id,
            session_id,
        },
        tool,
        trusted_sandbox_manifest,
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
        moa_core::SubAgentMessage::InitialTask(initial) => Ok(initial.parent_session),
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
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: None,
        }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(sequence_num)
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

/// Emits a `Failed` control-plane attention signal to the owning coordinator when a
/// child turn ends in a FAILED outcome.
///
/// This is the only child-originated control-plane emit in this increment: it is
/// low-frequency (once per failed turn) and routed to `parent_session`. The signal id
/// and timestamp are journaled via `ctx.run()`/`durable_utc_now` for replay safety, and
/// the cross-VO `record_child_signal` is dispatched detached (`.send()`) so the workflow
/// never blocks on the coordinator's single-writer queue. A missing `parent_session`
/// (failure before the first turn prepared) is non-fatal — the terminal-delivery
/// idle-wake still covers waking an idle parent.
// TODO(Task 6): add the model-driven report tool so the child loop can emit
// Finding/NeedsInput/Blocked signals (with the needs_input awakeable round-trip) here.
async fn emit_failed_child_signal_if_needed(
    ctx: &WorkflowContext<'_>,
    sub_agent_id: &str,
    parent_session: Option<SessionId>,
    outcome: &TurnOutcome,
) -> Result<(), HandlerError> {
    if !matches!(outcome.kind, TurnOutcomeKind::Failed) {
        return Ok(());
    }
    let Some(parent_session) = parent_session else {
        tracing::warn!(
            sub_agent_id = %sub_agent_id,
            "child turn failed before a parent session was known; skipping Failed control-plane signal (terminal idle-wake still applies)"
        );
        return Ok(());
    };

    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("sub_agent_failed_signal_id")
        .await?
        .into_inner();
    let created_at = durable_utc_now(ctx, "sub_agent_failed_signal_at").await?;
    let signal = build_failed_child_signal(
        sub_agent_id,
        parent_session,
        signal_id,
        created_at,
        &outcome.message,
    );
    // DETACHED: never block the workflow on the coordinator VO's single-writer queue.
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(signal))
        .send();
    tracing::info!(
        sub_agent_id = %sub_agent_id,
        parent_session = %parent_session,
        signal_id = %signal_id,
        "emitted Failed control-plane signal to coordinator"
    );
    Ok(())
}

/// Builds the `Failed` control-plane signal for a failed child turn.
///
/// A resume-eligible `Failed`/`Critical` signal with `resume_policy = IfIdle` and a
/// short, safe one-line summary. Kept pure (no Restate context) so the construction is
/// unit-testable; the caller journals `signal_id`/`created_at`.
fn build_failed_child_signal(
    sub_agent_id: &str,
    parent_session: SessionId,
    signal_id: AgentSignalId,
    created_at: DateTime<Utc>,
    failure_message: &str,
) -> SubAgentSignal {
    SubAgentSignal {
        signal_id,
        sub_agent_id: sub_agent_id.to_string(),
        parent_session,
        parent_sub_agent: None,
        kind: ChildSignalKind::Failed,
        severity: SignalSeverity::Critical,
        summary: short_failure_summary(failure_message),
        payload: serde_json::Value::Null,
        created_at,
        resume_policy: ParentResumePolicy::IfIdle,
        input_request_id: None,
        input_audience: None,
    }
}

/// Reduces a failure message into a short, safe one-line control-plane summary.
fn short_failure_summary(message: &str) -> String {
    const MAX_CHARS: usize = 200;
    let first_line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let base = if first_line.is_empty() {
        "sub-agent turn failed"
    } else {
        first_line
    };
    if base.chars().count() > MAX_CHARS {
        let truncated: String = base.chars().take(MAX_CHARS).collect();
        format!("{truncated}…")
    } else {
        base.to_string()
    }
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
    use moa_core::wire::turn::{TurnOutcome, TurnOutcomeKind};
    use moa_core::{AgentSignalId, ChildSignalKind, ParentResumePolicy, SessionId, SignalSeverity};

    use super::{
        build_failed_child_signal, parent_session_from_initial_message, short_failure_summary,
    };

    #[test]
    fn failed_outcome_builds_resume_eligible_failed_signal() {
        // Pins: a FAILED child turn constructs a Failed/Critical, IfIdle-resume signal
        // routed to the owning coordinator with a short summary derived from the failure.
        let parent_session = SessionId::new();
        let signal_id = AgentSignalId::new();
        let created_at = Utc::now();
        let outcome = TurnOutcome {
            turn_id: "turn-1".to_string(),
            kind: TurnOutcomeKind::Failed,
            message: "tool sandbox crashed\nstack trace line".to_string(),
        };

        let signal = build_failed_child_signal(
            "parent-1-child-1",
            parent_session,
            signal_id,
            created_at,
            &outcome.message,
        );

        assert_eq!(signal.signal_id, signal_id);
        assert_eq!(signal.sub_agent_id, "parent-1-child-1");
        assert_eq!(signal.parent_session, parent_session);
        assert_eq!(signal.kind, ChildSignalKind::Failed);
        assert_eq!(signal.severity, SignalSeverity::Critical);
        assert_eq!(signal.resume_policy, ParentResumePolicy::IfIdle);
        // Summary is the first non-empty line only (no multi-line leak).
        assert_eq!(signal.summary, "tool sandbox crashed");
    }

    #[test]
    fn short_failure_summary_falls_back_and_truncates() {
        // Pins: empty failure text yields a safe default; overlong text is truncated.
        assert_eq!(short_failure_summary("   "), "sub-agent turn failed");
        let long = "x".repeat(300);
        let summary = short_failure_summary(&long);
        assert!(summary.chars().count() <= 201, "summary must be bounded");
        assert!(summary.ends_with('…'), "overlong summary is truncated");
    }

    #[test]
    fn reserved_child_parent_session_requires_initial_message() {
        // Pins: nested spawn events derive their root session only from validated initial child messages.
        let session_id = SessionId::new();
        let message =
            moa_core::SubAgentMessage::InitialTask(Box::new(moa_core::SubAgentInitialTask {
                task: "inspect".to_string(),
                tool_subset: Vec::new(),
                budget_tokens: 100,
                max_turns: Some(2),
                parent_session: session_id,
                parent_sub_agent: Some("parent".to_string()),
                depth: 2,
                tenant_id: moa_core::TenantId::new(),
                user_id: moa_core::UserId::new("user"),
                model: moa_core::ModelId::new("model"),
                trusted_sandbox_manifest: None,
            }));

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
