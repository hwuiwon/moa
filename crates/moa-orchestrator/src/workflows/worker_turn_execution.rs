//! Workflow-backed execution for one worker turn run.
//!
//! The `Worker` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_core::wire::turn::{
    RunWorkerTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
};
use moa_core::{config::SessionLimitsConfig, traits::ChannelAdapter};
use moa_core::{
    coordination_counters::CoordinationCounters,
    coordination_counters::scope_coordination_counters, events::Event, types::channel::Channel,
    types::completion::CompletionContent, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::completion::StopReason,
    types::completion::TokenUsage, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::identifiers::AgentSignalId,
    types::identifiers::SessionId, types::identifiers::ToolCallId, types::provider::ModelTier,
    types::session::SessionMeta, types::session::TurnOutcome as CoreTurnOutcome,
    types::tools::ToolOutput, types::tools::TrustedSandboxFileManifestRef,
    types::worker::commands::ChildReportKind, types::worker::commands::ReportToParentInput,
    types::worker::commands::RequestInputInput, types::worker::commands::WorkerToolRecord,
    types::worker::commands::WorkerTurnOutcomeRecord,
    types::worker::commands::WorkerTurnPreparation,
    types::worker::commands::WorkerTurnResponseRecord, types::worker::state::ChildSignalKind,
    types::worker::state::InputAudience, types::worker::state::ParentResumePolicy,
    types::worker::state::SignalSeverity, types::worker::state::WorkerPendingInput,
    types::worker::state::WorkerSignal, types::worker::tool_schema::ChildReportTool,
};
use moa_observability::restate_observability::{
    annotate_restate_handler_span, emit_turn_coordination_summary, llm_call_span,
    tool_dispatch_span, worker_turn_span,
};
use moa_observability::{
    record_session_error, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
    record_turn_workflow_outcome,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::objects::session::SessionClient;
use crate::objects::worker::{MAX_WORKER_TURNS_PER_WORKFLOW, WorkerClient};
use crate::services::{llm_gateway::LLMGatewayClient, session_store::RestateSessionStoreClient};
use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationRequest,
    invoke_governed_tool, record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification, response_tool_calls,
    stable_tool_call_id, turn_outcome_for_response,
};
use crate::turn_driver::{
    model_loop as driver_model_loop, progress as driver_progress, segments as driver_segments,
};
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::{
    append_session_event, append_tool_call_event, append_tool_result_event,
    append_zero_cost_assistant_response, emit_tool_budget_exceeded,
    record_segment_skill_use_for_tool_call, record_segment_tool_use, turn_outcome_kind_label,
};
use crate::workflows::turn_execution::selected_procedure_skill_refs;
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL};
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState,
};
use moa_session::PostgresSessionStore;

#[derive(Clone, Debug)]
enum WorkerIterationOutcome {
    Core(CoreTurnOutcome),
    Cancelled(String),
    ToolBudgetExceeded(String),
}

struct WorkerIterationInput<'a> {
    request: &'a RunWorkerTurnRequest,
    completion_request: CompletionRequest,
    active_canary: Option<String>,
    meta: SessionMeta,
    parent_session: SessionId,
    turn_evidence: &'a mut TurnEvidence,
    tool_budget: &'a mut ToolBudgetState,
}

/// Restate workflow surface for durable worker turn execution.
#[restate_sdk::workflow]
pub trait WorkerTurnExecution {
    /// Runs one worker turn workflow body.
    async fn run(request: Json<RunWorkerTurnRequest>) -> Result<Json<TurnOutcome>, HandlerError>;

    /// Requests cancellation of the in-flight worker turn workflow.
    #[shared]
    async fn request_cancel(reason: Json<String>) -> Result<(), HandlerError>;

    /// Returns workflow progress without blocking the workflow body.
    #[shared]
    async fn progress() -> Result<Json<TurnProgress>, HandlerError>;
}

/// Concrete `WorkerTurnExecution` workflow implementation.
#[derive(Clone)]
pub struct WorkerTurnExecutionImpl {
    session_limits: SessionLimitsConfig,
    session_store: Arc<PostgresSessionStore>,
    channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl WorkerTurnExecutionImpl {
    /// Creates a worker-turn workflow with its limits and progress-delivery dependencies.
    #[must_use]
    pub fn new(
        session_limits: SessionLimitsConfig,
        session_store: Arc<PostgresSessionStore>,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
    ) -> Self {
        Self {
            session_limits,
            session_store,
            channel_adapters,
        }
    }
}

impl WorkerTurnExecution for WorkerTurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunWorkerTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        annotate_restate_handler_span("WorkerTurnExecution", "run");
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        let request = request.into_inner();
        driver_progress::set_phase(&ctx, TurnPhase::Compiling);

        // `parent_session` is learned during the loop (best-effort for the FAILED-signal
        // emit below). It stays `None` if the workflow errors before the first turn is
        // prepared; the terminal idle-wake still covers waking an idle parent.
        let mut parent_session: Option<SessionId> = None;
        let outcome =
            match run_worker_inside_workflow(self, &ctx, &request, &mut parent_session).await {
                Ok(outcome) => outcome,
                Err(error) => TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Failed,
                    message: format!("{error:?}"),
                },
            };
        record_turn_workflow_outcome(
            "worker",
            turn_outcome_kind_label(&outcome.kind),
            ModelTier::Auxiliary,
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
        emit_failed_child_signal_if_needed(&ctx, &request.worker_id, parent_session, &outcome)
            .await?;
        notify_worker_of_outcome(&ctx, &request.worker_id, &outcome);
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("WorkerTurnExecution", "request_cancel");
        driver_progress::request_cancel(&ctx, reason.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        annotate_restate_handler_span("WorkerTurnExecution", "progress");
        driver_progress::snapshot(&ctx).await
    }
}

async fn run_worker_inside_workflow(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
    parent_session_out: &mut Option<SessionId>,
) -> Result<TurnOutcome, HandlerError> {
    let session_limits = &workflow.session_limits;
    let loop_plan = driver_model_loop::worker_loop_plan(
        driver_model_loop::WorkerLoopPlanRequest {
            request_max_turns: request.max_turns,
            default_max_turns: MAX_WORKER_TURNS_PER_WORKFLOW,
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
            moa_core::coordination_counters::record_vo_send();
            ctx.object_client::<WorkerClient>(request.worker_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        driver_progress::set_phase(ctx, TurnPhase::Compiling);
        moa_core::coordination_counters::record_worker_vo_call();
        let preparation = ctx
            .object_client::<WorkerClient>(request.worker_id.clone())
            .prepare_turn()
            .call()
            .await?
            .into_inner();
        let (completion_request, active_canary, meta, parent_session) = match preparation {
            WorkerTurnPreparation::Outcome { outcome } => {
                return Ok(workflow_outcome_from_core(request, outcome));
            }
            WorkerTurnPreparation::Request {
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
        let turn_span = worker_turn_span(
            &meta,
            &request.worker_id,
            &request.turn_id,
            turn_number as i64,
            None,
        );
        let turn_coordination_counters = Arc::new(CoordinationCounters::default());
        let outcome = scope_coordination_counters(
            turn_coordination_counters.clone(),
            run_worker_iteration(
                workflow,
                ctx,
                WorkerIterationInput {
                    request,
                    completion_request,
                    active_canary,
                    meta,
                    parent_session,
                    turn_evidence: &mut turn_evidence,
                    tool_budget: &mut tool_budget,
                },
            )
            .instrument(turn_span.clone()),
        )
        .await?;
        let turn_coordination_snapshot = turn_coordination_counters.snapshot();
        emit_turn_coordination_summary(&turn_span, &turn_coordination_snapshot);
        match outcome {
            WorkerIterationOutcome::Cancelled(message) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message,
                });
            }
            WorkerIterationOutcome::ToolBudgetExceeded(message) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message,
                });
            }
            WorkerIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
            WorkerIterationOutcome::Core(CoreTurnOutcome::Idle) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Completed,
                    message: "worker turn completed".to_string(),
                });
            }
            WorkerIterationOutcome::Core(CoreTurnOutcome::Cancelled) => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Cancelled,
                    message: "worker turn cancelled".to_string(),
                });
            }
        }
    }

    if let (Some(meta), Some(parent_session)) = (last_request_meta.as_ref(), last_parent_session) {
        let message =
            record_worker_turn_cap_stop(ctx, request, meta, parent_session, max_turns).await?;
        return Ok(TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message,
        });
    }

    Ok(TurnOutcome {
        turn_id: request.turn_id.clone(),
        kind: TurnOutcomeKind::Failed,
        message: format!("worker model-loop turn cap reached ({max_turns})"),
    })
}

async fn run_worker_iteration(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    mut input: WorkerIterationInput<'_>,
) -> Result<WorkerIterationOutcome, HandlerError> {
    // The root segment's activated skills; a worker tool call that engages one of them
    // credits the root segment's `skills_used` (workers contribute to the root learning
    // unit), mirroring the root turn's skill-use recording.
    let selected_skills =
        attach_active_segment_metadata(ctx, input.parent_session, &mut input.completion_request)
            .await?;
    let allowed_tools = allowed_tool_names(&input.completion_request);
    // Captured before the completion request is moved into the model call below, so a
    // worker `run_procedure` call is gated by the same selected-skill set as the root.
    let selected_procedure_skills =
        selected_procedure_skill_refs(&input.completion_request.metadata);

    driver_progress::set_phase(ctx, TurnPhase::Streaming);
    turn_progress::maybe_emit(
        ctx,
        input.parent_session,
        SUMMARY_CALLING_MODEL,
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    record_worker_heartbeat(ctx, &input.request.worker_id).await?;
    let span = llm_call_span(&input.meta);
    let llm_started = Instant::now();
    let response = {
        let _guard = span.enter();
        restate_sdk::select! {
            reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                moa_core::coordination_counters::record_vo_send();
                ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
                    .cancel(reason.clone())
                    .send();
                return Ok(WorkerIterationOutcome::Cancelled(reason));
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

    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
        .record_response(Json::from(WorkerTurnResponseRecord {
            turn_id: input.request.turn_id.clone(),
            response: response.clone(),
        }))
        .call()
        .await?;

    if verification_annotated {
        let outcome = CoreTurnOutcome::Idle;
        moa_core::coordination_counters::record_worker_vo_call();
        ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
            .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
                turn_id: input.request.turn_id.clone(),
                outcome,
            }))
            .call()
            .await?;
        return Ok(WorkerIterationOutcome::Core(outcome));
    }

    for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            moa_core::coordination_counters::record_vo_send();
            ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
                .cancel(reason.clone())
                .send();
            return Ok(WorkerIterationOutcome::Cancelled(reason));
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
                let message = record_worker_budget_stop(
                    ctx,
                    input.request,
                    &input.meta,
                    input.parent_session,
                    &exhaustion,
                )
                .await?;
                return Ok(WorkerIterationOutcome::ToolBudgetExceeded(message));
            }
        }
        let tool_context = WorkerToolContext {
            turn_id: &input.request.turn_id,
            worker_id: &input.request.worker_id,
            meta: &input.meta,
            session_id: input.parent_session,
            active_canary: input.active_canary.as_deref(),
            trusted_sandbox_manifest: input.request.trusted_sandbox_manifest.as_ref(),
            selected_procedure_skills: &selected_procedure_skills,
            selected_skills: &selected_skills,
        };
        handle_tool_call(
            workflow,
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
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
        .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
            turn_id: input.request.turn_id.clone(),
            outcome,
        }))
        .call()
        .await?;
    Ok(WorkerIterationOutcome::Core(outcome))
}

/// Refreshes the child's telemetry-plane heartbeat at the progress cadence.
///
/// The timestamp is journaled via `durable_utc_now` so it stays replay-stable, then
/// fire-and-forget delivered to the `Worker` VO. This is VO state only (no event
/// per tick); the watchdog and `progress_summary` read it to detect a stuck child.
async fn record_worker_heartbeat(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx, "worker_heartbeat").await?;
    moa_core::coordination_counters::record_vo_send();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .record_heartbeat(Json::from(now))
        .send();
    Ok(())
}

/// Attaches the active segment's metadata to a worker completion request and returns the
/// skills the root turn activated on that segment.
///
/// A worker inherits the root's trusted sandbox manifest (delegation copies it), so a
/// worker tool call can engage a root-injected skill by reading its materialized
/// `.moa/skills/<slug>/` package. The returned names are the same `skills_activated` set
/// attribution compares against `skills_used`, so the caller runs skill-use detection
/// against them to credit the root segment for skills a worker actually engaged. Returns an
/// empty vector when the session has no active segment.
async fn attach_active_segment_metadata(
    ctx: &WorkflowContext<'_>,
    parent_session: SessionId,
    request: &mut CompletionRequest,
) -> Result<Vec<String>, HandlerError> {
    let Some(segment) = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(parent_session))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view())
    else {
        return Ok(Vec::new());
    };
    driver_segments::insert_active_segment_metadata(request, &segment);
    Ok(segment.skills_activated)
}

struct WorkerToolContext<'a> {
    turn_id: &'a str,
    worker_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    active_canary: Option<&'a str>,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    selected_procedure_skills: &'a BTreeSet<String>,
    /// Skills the root turn activated on the active segment, used to detect which a worker
    /// tool call engaged so worker skill use is credited to the root segment.
    selected_skills: &'a [String],
}

async fn handle_tool_call(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: WorkerToolContext<'_>,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let worker_id = tool_context.worker_id;
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let selected_skills = tool_context.selected_skills;
    let tool_id = stable_tool_call_id(session_id, index, tool_call);

    // Child-only report/request-input tools are handled in the child's own turn loop, not
    // via the governed executor or the delegation-manager path: they emit control-plane
    // signals up to the owning coordinator (and, for `request_input`, block the child turn
    // on an awakeable round-trip mirroring `wait_worker`).
    if let Some(report_tool) = ChildReportTool::from_invocation(&tool_call.invocation)
        .map_err(|error| TerminalError::new(error.to_string()))?
    {
        return handle_child_report_tool(
            workflow,
            ctx,
            ChildReportToolRequest {
                turn_id: tool_context.turn_id,
                worker_id,
                parent_session: session_id,
                tool_id,
                tool_call,
                report_tool,
            },
            turn_evidence,
        )
        .await;
    }

    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: meta,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            selected_procedure_skills: tool_context.selected_procedure_skills,
            active_canary: tool_context.active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::Worker {
                worker_id,
                turn_id: tool_context.turn_id,
            },
        },
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;

    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            if result.should_record_denied_worker_tool() {
                record_denied_tool(
                    ctx,
                    tool_context.turn_id,
                    worker_id,
                    result.tool_id,
                    &result.invocation,
                    &result.output,
                )
                .await?;
            } else {
                record_tool_result(
                    ctx,
                    tool_context.turn_id,
                    worker_id,
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
            // Credit the root segment for any root-activated skill this worker tool call
            // engaged. Worker tool use already aggregates into the root segment above, so
            // recording skill use here keeps `skills_used` in the same scope as `tools_used`;
            // otherwise a skill a worker read/ran is misclassified as an unused injection.
            record_segment_skill_use_for_tool_call(
                ctx,
                session_id,
                &result.invocation.name,
                &result.invocation.input,
                selected_skills,
            )
            .await?;
        }
        GovernedInvocationOutcome::Delegation { tool_id, .. } => {
            handle_delegation_tool(
                workflow,
                ctx,
                WorkerDelegationToolRequest {
                    turn_id: tool_context.turn_id,
                    worker_id,
                    session_id,
                    tool_id,
                    tool_call,
                },
                turn_evidence,
            )
            .await?;
        }
    }
    Ok(())
}

struct WorkerDelegationToolRequest<'a> {
    turn_id: &'a str,
    worker_id: &'a str,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &'a ToolCallContent,
}

async fn handle_delegation_tool(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: WorkerDelegationToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let WorkerDelegationToolRequest {
        turn_id,
        worker_id,
        session_id,
        tool_id,
        tool_call,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    // Workers are never granted delegation tools, so any delegation-named call reaching here is a
    // model hallucination. Return a graceful, recoverable tool error WITHOUT parsing the
    // (possibly malformed) invocation — parsing and erroring on it would fail the whole worker
    // turn instead of steering the model back on task.

    let span = tool_dispatch_span(&invocation.name);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_progress::running_tool_summary(&invocation.name),
        &workflow.session_limits,
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    record_worker_heartbeat(ctx, worker_id).await?;
    let dispatch_started = Instant::now();
    let output = async {
        Ok::<_, HandlerError>(ToolOutput::error(
            "workers cannot manage other workers",
            Duration::ZERO,
        ))
    }
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);

    append_tool_result_event(ctx, session_id, tool_id, &invocation, &output).await?;
    record_denied_tool(ctx, turn_id, worker_id, tool_id, &invocation, &output).await?;
    turn_evidence.record_tool_result(&invocation, &output);
    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

/// One child-only report tool invocation routed inside the child's own turn loop.
struct ChildReportToolRequest<'a> {
    turn_id: &'a str,
    worker_id: &'a str,
    parent_session: SessionId,
    tool_id: ToolCallId,
    tool_call: &'a ToolCallContent,
    report_tool: ChildReportTool,
}

/// Handles a child-only `report_to_parent`/`request_input` tool call.
///
/// Mirrors `handle_delegation_tool`'s event bookkeeping (tool-call event, child-history
/// tool result, evidence) so the child's conversation stays consistent, but the work is a
/// control-plane emit to the owning coordinator rather than a managed-child operation.
/// `report_to_parent` returns immediately; `request_input` blocks the child turn on a
/// Restate awakeable until the coordinator answers (`ProvideInput`) or the long timeout
/// elapses.
async fn handle_child_report_tool(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: ChildReportToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let ChildReportToolRequest {
        turn_id,
        worker_id,
        parent_session,
        tool_id,
        tool_call,
        report_tool,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, parent_session, tool_id, tool_call).await?;
    record_worker_heartbeat(ctx, worker_id).await?;
    let output = match report_tool {
        ChildReportTool::Report(input) => {
            report_to_parent(ctx, worker_id, parent_session, &input).await?
        }
        ChildReportTool::RequestInput(input) => {
            request_input_from_parent(workflow, ctx, worker_id, parent_session, &input).await?
        }
    };
    append_tool_result_event(ctx, parent_session, tool_id, &invocation, &output).await?;
    record_tool_result(ctx, turn_id, worker_id, tool_id, &invocation, &output).await?;
    turn_evidence.record_tool_result(&invocation, &output);
    Ok(())
}

/// Emits a model-driven `Finding`/`Blocked` control-plane signal to the coordinator.
///
/// `signal_id`/`created_at` are journaled (`ctx.run`/`durable_utc_now`) for replay safety
/// and the cross-VO `record_child_signal` is dispatched detached (`.send()`) so the child
/// turn never blocks on the coordinator's single-writer queue. A `Finding` records without
/// arming a resume (`ParentResumePolicy::Never`); a `Blocked` is resume-eligible
/// (`IfIdle`) and can wake an idle coordinator.
async fn report_to_parent(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    parent_session: SessionId,
    input: &ReportToParentInput,
) -> Result<ToolOutput, HandlerError> {
    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("child_report_signal_id")
        .await?
        .into_inner();
    let created_at = durable_utc_now(ctx, "child_report_signal_at").await?;
    let signal = build_child_report_signal(worker_id, parent_session, signal_id, created_at, input);
    moa_core::coordination_counters::record_vo_send();
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(signal))
        .send();
    tracing::info!(
        worker_id = %worker_id,
        parent_session = %parent_session,
        signal_id = %signal_id,
        kind = ?input.kind,
        "child reported to coordinator"
    );
    Ok(ToolOutput::text(
        format!("Reported {} to the coordinator.", input.kind.label()),
        Duration::ZERO,
    ))
}

/// Runs the child `request_input` awakeable round-trip and returns the answer (or a
/// timeout result).
///
/// Mirrors the `wait_worker` awakeable pattern with the roles reversed: the child turn
/// workflow registers an awakeable, stores `(input_request_id → awakeable_id)` on its own
/// `Worker` VO, emits a `NeedsInput` signal (which arms an idle-coordinator resume), then
/// `select!`s the awakeable against a long timeout. A later
/// `Worker::post_message(ProvideInput)` resolves the awakeable from the coordinator's
/// answer. On timeout the mapping is cleared so a late `ProvideInput` is an idempotent
/// no-op, and the child receives a "no input" result so it can proceed or report blocked.
async fn request_input_from_parent(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    parent_session: SessionId,
    input: &RequestInputInput,
) -> Result<ToolOutput, HandlerError> {
    let input_request_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(uuid::Uuid::now_v7().to_string())) })
        .name("child_input_request_id")
        .await?
        .into_inner();
    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("child_input_signal_id")
        .await?
        .into_inner();
    let created_at = durable_utc_now(ctx, "child_input_signal_at").await?;
    let (awakeable_id, answer_future) = ctx.awakeable::<String>();

    // Persist the awakeable mapping on the child VO BEFORE emitting the signal so any
    // `ProvideInput` the coordinator sends in response always finds it (this `.call()`
    // awaits durable storage). Mirrors `attach_result_waiter` in the wait path.
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .register_input_request(Json::from(WorkerPendingInput {
            input_request_id: input_request_id.clone(),
            awakeable_id,
        }))
        .call()
        .await?;

    // Emit the NeedsInput signal to the owning coordinator (arms a guarded resume if the
    // coordinator is idle). DETACHED so the child never blocks on the coordinator's queue.
    //
    // TODO(User-audience wiring): for `input_audience = User` the coordinator must surface
    // the question to the human and forward the human's reply as `ProvideInput`. The
    // awakeable + `ProvideInput` mechanism below is already complete, so User routing is a
    // thin addition (coordinator prompt guidance + edge user-reply forwarding) that depends
    // on the edge work in Task 10; today the question/audience are visible to the
    // coordinator via the recorded `NeedsInput` signal and it can answer via
    // `provide_worker_input`.
    let signal = build_needs_input_signal(
        worker_id,
        parent_session,
        signal_id,
        created_at,
        &input_request_id,
        input.audience,
        &input.question,
    );
    moa_core::coordination_counters::record_vo_send();
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(signal))
        .send();

    let timeout_ms = workflow.session_limits.worker_input_timeout_ms;
    let output = restate_sdk::select! {
        answer = answer_future => {
            ToolOutput::text(format!("Input received: {}", answer?), Duration::ZERO)
        },
        _ = ctx.sleep(Duration::from_millis(timeout_ms)) => {
            // Clear the now-dead mapping so a late ProvideInput is an idempotent no-op.
            moa_core::coordination_counters::record_worker_vo_call();
            ctx.object_client::<WorkerClient>(worker_id.to_string())
                .clear_input_request(Json::from(input_request_id.clone()))
                .call()
                .await?;
            ToolOutput::text(
                "No input was received in time. Proceed with your best judgment or report that you are blocked."
                    .to_string(),
                Duration::ZERO,
            )
        }
    };
    Ok(output)
}

/// Builds the `Finding`/`Blocked` control-plane signal for a model-driven child report.
///
/// Kept pure (no Restate context) so the resume policy mapping is unit-testable: a
/// `Finding` records without waking the coordinator (`ParentResumePolicy::Never`), a
/// `Blocked` is resume-eligible (`IfIdle`). The caller journals `signal_id`/`created_at`.
fn build_child_report_signal(
    worker_id: &str,
    parent_session: SessionId,
    signal_id: AgentSignalId,
    created_at: DateTime<Utc>,
    input: &ReportToParentInput,
) -> WorkerSignal {
    let (kind, severity, resume_policy) = match input.kind {
        ChildReportKind::Finding => (
            ChildSignalKind::Finding,
            SignalSeverity::Info,
            ParentResumePolicy::Never,
        ),
        ChildReportKind::Blocked => (
            ChildSignalKind::Blocked,
            SignalSeverity::Warning,
            ParentResumePolicy::IfIdle,
        ),
    };
    WorkerSignal {
        signal_id,
        worker_id: worker_id.to_string(),
        parent_session,
        kind,
        severity,
        summary: clamp_signal_summary(&input.summary, "worker report"),
        payload: serde_json::Value::Null,
        created_at,
        resume_policy,
        input_request_id: None,
        input_audience: None,
    }
}

/// Builds the `NeedsInput` control-plane signal for a child `request_input` round-trip.
///
/// Kept pure so the carried `input_request_id`/`input_audience` and the resume-eligible
/// `IfIdle` policy are unit-testable. The caller journals `signal_id`/`created_at` and owns
/// the awakeable lifecycle.
#[allow(clippy::too_many_arguments)]
fn build_needs_input_signal(
    worker_id: &str,
    parent_session: SessionId,
    signal_id: AgentSignalId,
    created_at: DateTime<Utc>,
    input_request_id: &str,
    audience: InputAudience,
    question: &str,
) -> WorkerSignal {
    WorkerSignal {
        signal_id,
        worker_id: worker_id.to_string(),
        parent_session,
        kind: ChildSignalKind::NeedsInput,
        severity: SignalSeverity::Warning,
        summary: clamp_signal_summary(question, "worker requested input"),
        payload: serde_json::Value::Null,
        created_at,
        resume_policy: ParentResumePolicy::IfIdle,
        input_request_id: Some(input_request_id.to_string()),
        input_audience: Some(audience),
    }
}

async fn record_worker_budget_stop(
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<String, HandlerError> {
    emit_tool_budget_exceeded(ctx, parent_session, exhaustion).await?;
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
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(request.worker_id.clone())
        .record_response(Json::from(WorkerTurnResponseRecord {
            turn_id: request.turn_id.clone(),
            response,
        }))
        .call()
        .await?;
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(request.worker_id.clone())
        .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
            turn_id: request.turn_id.clone(),
            outcome: CoreTurnOutcome::Idle,
        }))
        .call()
        .await?;
    Ok(message)
}

async fn record_worker_turn_cap_stop(
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    max_turns: usize,
) -> Result<String, HandlerError> {
    record_session_error("turn_cap");
    append_session_event(
        ctx,
        parent_session,
        Event::Error {
            message: format!("worker model-loop turn cap reached ({max_turns}), stopping"),
            recoverable: true,
        },
    )
    .await?;
    let message = format!(
        "MOA stopped because this worker reached the model-loop turn cap ({max_turns}). Narrow the scope or ask MOA to continue."
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
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(request.worker_id.clone())
        .record_response(Json::from(WorkerTurnResponseRecord {
            turn_id: request.turn_id.clone(),
            response,
        }))
        .call()
        .await?;
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(request.worker_id.clone())
        .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
            turn_id: request.turn_id.clone(),
            outcome: CoreTurnOutcome::Idle,
        }))
        .call()
        .await?;
    Ok(message)
}

async fn record_tool_result(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    worker_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .record_tool_result(Json::from(WorkerToolRecord {
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
    worker_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .record_denied_tool(Json::from(WorkerToolRecord {
            turn_id: Some(turn_id.to_string()),
            tool_id,
            invocation: invocation.clone(),
            output: output.clone(),
        }))
        .call()
        .await?;
    Ok(())
}

fn notify_worker_of_outcome(ctx: &WorkflowContext<'_>, worker_id: &str, outcome: &TurnOutcome) {
    moa_core::coordination_counters::record_vo_send();
    ctx.object_client::<WorkerClient>(worker_id.to_string())
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
// Model-driven Finding/Blocked/NeedsInput signals (incl. the needs_input awakeable
// round-trip) are emitted from inside the child turn loop by `handle_child_report_tool`;
// this terminal `Failed` emit covers a turn that errors out.
async fn emit_failed_child_signal_if_needed(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    parent_session: Option<SessionId>,
    outcome: &TurnOutcome,
) -> Result<(), HandlerError> {
    if !matches!(outcome.kind, TurnOutcomeKind::Failed) {
        return Ok(());
    }
    let Some(parent_session) = parent_session else {
        tracing::warn!(
            worker_id = %worker_id,
            "child turn failed before a parent session was known; skipping Failed control-plane signal (terminal idle-wake still applies)"
        );
        return Ok(());
    };

    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("worker_failed_signal_id")
        .await?
        .into_inner();
    let created_at = durable_utc_now(ctx, "worker_failed_signal_at").await?;
    let signal = build_failed_child_signal(
        worker_id,
        parent_session,
        signal_id,
        created_at,
        &outcome.message,
    );
    // DETACHED: never block the workflow on the coordinator VO's single-writer queue.
    moa_core::coordination_counters::record_vo_send();
    ctx.object_client::<SessionClient>(parent_session.to_string())
        .record_child_signal(Json::from(signal))
        .send();
    tracing::info!(
        worker_id = %worker_id,
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
    worker_id: &str,
    parent_session: SessionId,
    signal_id: AgentSignalId,
    created_at: DateTime<Utc>,
    failure_message: &str,
) -> WorkerSignal {
    WorkerSignal {
        signal_id,
        worker_id: worker_id.to_string(),
        parent_session,
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
    clamp_signal_summary(message, "worker turn failed")
}

/// Reduces an arbitrary model-supplied string to a short, safe one-line signal summary.
///
/// Takes the first non-empty line (so multi-line tool output never leaks into a signal),
/// falls back to `fallback` when empty, and truncates to a bounded length so signals stay
/// compact on the coordinator VO.
fn clamp_signal_summary(message: &str, fallback: &str) -> String {
    const MAX_CHARS: usize = 200;
    let first_line = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let base = if first_line.is_empty() {
        fallback
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
    request: &RunWorkerTurnRequest,
    outcome: CoreTurnOutcome,
) -> TurnOutcome {
    match outcome {
        CoreTurnOutcome::Continue | CoreTurnOutcome::Idle => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Completed,
            message: match outcome {
                CoreTurnOutcome::Continue => "worker turn yielded continuation".to_string(),
                CoreTurnOutcome::Idle => "worker turn completed".to_string(),
                CoreTurnOutcome::Cancelled => unreachable!(),
            },
        },
        CoreTurnOutcome::Cancelled => TurnOutcome {
            turn_id: request.turn_id.clone(),
            kind: TurnOutcomeKind::Cancelled,
            message: "worker turn cancelled".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::wire::turn::{TurnOutcome, TurnOutcomeKind};
    use moa_core::{
        types::identifiers::AgentSignalId, types::identifiers::SessionId,
        types::worker::commands::ChildReportKind, types::worker::commands::ReportToParentInput,
        types::worker::state::ChildSignalKind, types::worker::state::InputAudience,
        types::worker::state::ParentResumePolicy, types::worker::state::SignalSeverity,
    };

    use super::{
        build_child_report_signal, build_failed_child_signal, build_needs_input_signal,
        short_failure_summary,
    };

    #[test]
    fn report_finding_records_without_arming_resume() {
        // Pins: a model-driven `finding` report builds a Finding/Info signal whose resume
        // policy is `Never`, so it records on the coordinator without waking it.
        let signal = build_child_report_signal(
            "parent-1-child-1",
            SessionId::new(),
            AgentSignalId::new(),
            Utc::now(),
            &ReportToParentInput {
                kind: ChildReportKind::Finding,
                summary: "found 2 of 3 plan tiers".to_string(),
            },
        );
        assert_eq!(signal.kind, ChildSignalKind::Finding);
        assert_eq!(signal.severity, SignalSeverity::Info);
        assert_eq!(
            signal.resume_policy,
            ParentResumePolicy::Never,
            "a finding must not arm a coordinator resume"
        );
        assert_eq!(signal.summary, "found 2 of 3 plan tiers");
        assert!(signal.input_request_id.is_none());
    }

    #[test]
    fn report_blocked_arms_resume_when_idle() {
        // Pins: a model-driven `blocked` report builds a Blocked/Warning signal with an
        // `IfIdle` resume policy so an idle coordinator can be woken to intervene.
        let signal = build_child_report_signal(
            "parent-1-child-1",
            SessionId::new(),
            AgentSignalId::new(),
            Utc::now(),
            &ReportToParentInput {
                kind: ChildReportKind::Blocked,
                summary: "cannot reach the billing API".to_string(),
            },
        );
        assert_eq!(signal.kind, ChildSignalKind::Blocked);
        assert_eq!(signal.severity, SignalSeverity::Warning);
        assert_eq!(signal.resume_policy, ParentResumePolicy::IfIdle);
    }

    #[test]
    fn request_input_builds_needs_input_signal_with_request_id_and_audience() {
        // Pins: request_input builds a NeedsInput/IfIdle signal that carries the
        // input_request_id and audience so the coordinator can answer the right request.
        let signal = build_needs_input_signal(
            "parent-1-child-1",
            SessionId::new(),
            AgentSignalId::new(),
            Utc::now(),
            "req-42",
            InputAudience::User,
            "Which staging cluster should I deploy to?",
        );
        assert_eq!(signal.kind, ChildSignalKind::NeedsInput);
        assert_eq!(signal.resume_policy, ParentResumePolicy::IfIdle);
        assert_eq!(signal.input_request_id.as_deref(), Some("req-42"));
        assert_eq!(signal.input_audience, Some(InputAudience::User));
        assert_eq!(signal.summary, "Which staging cluster should I deploy to?");
    }

    #[test]
    fn request_input_summary_is_first_line_bounded() {
        // Pins: an overlong / multi-line question is reduced to a bounded first line so a
        // NeedsInput signal never leaks raw multi-line content onto the coordinator VO.
        let question = format!("{}\nsecond line", "q".repeat(300));
        let signal = build_needs_input_signal(
            "child",
            SessionId::new(),
            AgentSignalId::new(),
            Utc::now(),
            "req-1",
            InputAudience::Coordinator,
            &question,
        );
        assert!(signal.summary.chars().count() <= 201);
        assert!(signal.summary.ends_with('…'));
        assert!(!signal.summary.contains("second line"));
    }

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
        assert_eq!(signal.worker_id, "parent-1-child-1");
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
        assert_eq!(short_failure_summary("   "), "worker turn failed");
        let long = "x".repeat(300);
        let summary = short_failure_summary(&long);
        assert!(summary.chars().count() <= 201, "summary must be bounded");
        assert!(summary.ends_with('…'), "overlong summary is truncated");
    }
}
