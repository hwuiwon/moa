//! Workflow-backed execution for one worker turn run.
//!
//! The `Worker` virtual object owns conversational state and message
//! admission. This workflow owns the repeated LLM/tool loop so `post_message`
//! can return quickly and child execution has a durable progress/cancellation
//! surface like top-level session turns.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_config::SessionLimitsConfig;
use moa_core::traits::ChannelAdapter;
use moa_core::{
    coordination_counters::CoordinationCounters,
    coordination_counters::scope_coordination_counters, events::Event, events::TurnFailureActor,
    events::TurnFailureClass, types::channel::Channel, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::StopReason, types::completion::TokenUsage,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::identifiers::AgentSignalId, types::identifiers::SessionId,
    types::identifiers::ToolCallId, types::provider::ModelTier, types::session::SessionMeta,
    types::session::TurnOutcome as CoreTurnOutcome, types::tools::ToolOutput,
    types::tools::TrustedSandboxFileManifestRef, types::worker::commands::ChildReportKind,
    types::worker::commands::ReportToParentInput, types::worker::commands::RequestInputInput,
    types::worker::commands::WorkerToolRecord, types::worker::commands::WorkerTurnOutcomeRecord,
    types::worker::commands::WorkerTurnPreparation,
    types::worker::commands::WorkerTurnResponseRecord, types::worker::signals::ChildSignalKind,
    types::worker::signals::ParentResumePolicy, types::worker::signals::SignalSeverity,
    types::worker::signals::WorkerSignal, types::worker::state::InputAudience,
    types::worker::state::WorkerInputRequest, types::worker::state::WorkerInputTarget,
    types::worker::state::WorkerPendingInput, types::worker::tool_schema::ChildReportTool,
};
use moa_hands::truncate_tool_span_text;
use moa_observability::restate_observability::{
    annotate_restate_handler_span, emit_turn_coordination_summary, llm_call_span,
    tool_dispatch_span, worker_turn_span,
};
use moa_observability::{
    record_session_error, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
    record_turn_workflow_outcome,
};
use moa_wire::turn::{RunWorkerTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::objects::session::SessionClient;
use crate::objects::worker::{
    MAX_WORKER_TURNS_PER_WORKFLOW, WorkerClearInputRequest, WorkerClient,
};
use crate::services::{
    llm_gateway::{
        LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient, attach_completion_owner,
        cancel_completion_owner, completion_idempotency_key,
    },
    session_store::RestateSessionStoreClient,
};
use crate::tool_invocation::governed::completion_tool_catalog_pin;
use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationRequest,
    invoke_governed_tool, record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification,
    exclude_reserved_control_tool_schemas, response_tool_calls, stable_worker_tool_call_id,
    turn_outcome_for_response,
};
use crate::turn_driver::{
    model_loop as driver_model_loop, progress as driver_progress, segments as driver_segments,
};
use crate::workflows::child_invocation::{ChildInvocationOutcome, cancel_and_join_child_call};
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::{
    TurnEventAppender, append_session_event, append_session_event_with_dedupe_key,
    append_tool_call_event, append_tool_result_event, append_turn_failed,
    append_zero_cost_assistant_response, emit_tool_budget_exceeded,
    record_segment_skill_use_for_tool_call, record_segment_tool_use, turn_outcome_kind_label,
};
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
    /// The prompt-injection circuit halted this worker turn.
    SecurityHalt,
}

struct WorkerIterationInput<'a> {
    request: &'a RunWorkerTurnRequest,
    completion_request: CompletionRequest,
    tool_catalog_pin: moa_hands::ToolCatalogPin,
    active_canary: Option<String>,
    meta: SessionMeta,
    parent_session: SessionId,
    model_turn: usize,
    turn_evidence: &'a mut TurnEvidence,
    tool_budget: &'a mut ToolBudgetState,
    disabled_capabilities: &'a mut BTreeMap<String, moa_core::types::security::ToolCapabilityId>,
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
    event_appender: TurnEventAppender,
}

impl WorkerTurnExecutionImpl {
    /// Creates a worker-turn workflow with its limits, event-append, and progress-delivery dependencies.
    #[must_use]
    pub fn new(
        session_limits: SessionLimitsConfig,
        session_store: Arc<PostgresSessionStore>,
        channel_adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
        event_appender: TurnEventAppender,
    ) -> Self {
        Self {
            session_limits,
            session_store,
            channel_adapters,
            event_appender,
        }
    }

    /// Returns the durable event-append dependency this workflow owns.
    fn event_appender(&self) -> &TurnEventAppender {
        &self.event_appender
    }
}

impl WorkerTurnExecution for WorkerTurnExecutionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunWorkerTurnRequest>,
    ) -> Result<Json<TurnOutcome>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("WorkerTurnExecution", "run");
        let request = request.into_inner();
        driver_progress::set_phase(&ctx, TurnPhase::Compiling);

        // The dispatching worker object supplies its owning session on the request,
        // so a failure before the first prepared iteration can still append the
        // parent-session facts below.
        let parent_session = request.parent_session;
        let mut outcome = match run_worker_inside_workflow(self, &ctx, &request).await {
            Ok(outcome) => outcome,
            Err(error) => {
                // The error is logged for operators and never persisted: it can
                // carry provider, tool, and prompt material.
                let error = truncate_tool_span_text(format!("{error:?}"));
                tracing::error!(
                    worker_id = %request.worker_id,
                    parent_session = %parent_session,
                    turn_id = %request.turn_id,
                    error = %error,
                    "worker turn workflow failed at its catch-all boundary"
                );
                TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Failed,
                    message: String::new(),
                }
            }
        };
        outcome = enforce_worker_user_message_origin(&request.turn_id, outcome);
        // One canonical fact for every failed worker turn, whatever produced it:
        // the catch-all boundary, a reported failure, or the origin invariant. It
        // is appended before the attention signal and the owner callback, so the
        // failure survives losing either, and its dedupe key collapses a replay
        // into the same single event. The outcome keeps an authored stable
        // rejection code when one is present; every other failure carries only
        // the fixed class sentence the append returns.
        if matches!(outcome.kind, TurnOutcomeKind::Failed) {
            // Attribute from the phase the turn died in, read before the terminal
            // phase below overwrites it.
            let class = TurnFailureClass::from(driver_progress::current_phase(&ctx).await?);
            let summary = append_turn_failed(
                self.event_appender(),
                &ctx,
                parent_session,
                TurnFailureActor::Worker {
                    worker_id: request.worker_id.clone(),
                },
                &request.turn_id,
                class,
            )
            .await?;
            if super::turn_events::safe_terminal_rejection_code(&outcome.message).is_none() {
                outcome.message = summary;
            }
        }
        record_turn_workflow_outcome(
            "worker",
            turn_outcome_kind_label(&outcome.kind),
            ModelTier::Auxiliary,
        );
        let phase = match outcome.kind {
            TurnOutcomeKind::Completed => TurnPhase::Completed,
            TurnOutcomeKind::Accepted { .. } => TurnPhase::Failed,
            TurnOutcomeKind::Cancelled => TurnPhase::Cancelled,
            TurnOutcomeKind::Failed => TurnPhase::Failed,
        };
        turn_progress::finish(&ctx).await?;
        driver_progress::set_phase(&ctx, phase);
        notify_worker_of_outcome(&ctx, &request.worker_id, &outcome).await?;
        Ok(Json::from(outcome))
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: SharedWorkflowContext<'_>,
        reason: Json<String>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("WorkerTurnExecution", "request_cancel");
        cancel_completion_owner(&ctx, LLMCompletionOwner::worker_turn(ctx.key())).await?;
        driver_progress::request_cancel(&ctx, reason.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("WorkerTurnExecution", "progress");
        driver_progress::snapshot(&ctx).await
    }
}

fn enforce_worker_user_message_origin(turn_id: &str, outcome: TurnOutcome) -> TurnOutcome {
    if matches!(outcome.kind, TurnOutcomeKind::Accepted { .. }) {
        return TurnOutcome {
            turn_id: turn_id.to_string(),
            kind: TurnOutcomeKind::Failed,
            message: "run_requires_user_message_origin".to_string(),
        };
    }
    outcome
}

async fn run_worker_inside_workflow(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
) -> Result<TurnOutcome, HandlerError> {
    let session_limits = &workflow.session_limits;
    // A review continuation reports one settled result back into the worker's own
    // history; it does not reopen the delegated task. So it runs exactly one
    // iteration with no tools, no matter what the worker's normal cap allows.
    let continuing_action_review = request.action_review.is_some();
    let loop_plan = driver_model_loop::worker_loop_plan(
        driver_model_loop::WorkerLoopPlanRequest {
            request_max_turns: if continuing_action_review {
                Some(1)
            } else {
                request.max_turns
            },
            default_max_turns: MAX_WORKER_TURNS_PER_WORKFLOW,
        },
        session_limits,
    );
    let max_turns = loop_plan.max_turns;
    let mut tool_budget = loop_plan.tool_budget();
    let mut disabled_capabilities =
        BTreeMap::<String, moa_core::types::security::ToolCapabilityId>::new();
    driver_progress::initialize_loop_progress(
        ctx,
        loop_plan.route,
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
            return Ok(TurnOutcome {
                turn_id: request.turn_id.clone(),
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
            });
        }

        driver_progress::set_phase(ctx, TurnPhase::Compiling);
        moa_core::coordination_counters::record_worker_vo_call();
        let preparation = crate::restate_identity::replay_safe_request(
            ctx.object_client::<WorkerClient>(request.worker_id.clone())
                .prepare_turn(),
        )
        .call()
        .await?
        .into_inner();
        let (completion_request, tool_catalog_pin, active_canary, meta, parent_session) =
            match preparation {
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
                    let mut completion_request = *request;
                    let mut active_canary = active_canary;
                    if continuing_action_review {
                        // No tools on a continuation, so there is nothing for a canary to
                        // protect and nothing that could raise a second review from here.
                        completion_request.tools.clear();
                        active_canary = None;
                    }
                    let tool_catalog_pin = completion_tool_catalog_pin(&completion_request)?;
                    (
                        completion_request,
                        tool_catalog_pin,
                        active_canary,
                        *session_meta,
                        parent_session,
                    )
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
                    tool_catalog_pin,
                    active_canary,
                    meta,
                    parent_session,
                    model_turn: turn_number,
                    turn_evidence: &mut turn_evidence,
                    tool_budget: &mut tool_budget,
                    disabled_capabilities: &mut disabled_capabilities,
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
            // A halted worker turn is a failure. Reporting `Failed` is what makes
            // the existing terminal path emit exactly one `Failed` control-plane
            // signal and end this worker turn — no second signal writer here.
            WorkerIterationOutcome::SecurityHalt => {
                return Ok(TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: TurnOutcomeKind::Failed,
                    message: WORKER_SECURITY_CIRCUIT_HALT_MESSAGE.to_string(),
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
        let message = record_worker_turn_cap_stop(
            workflow.event_appender(),
            ctx,
            request,
            meta,
            parent_session,
            max_turns,
        )
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
    let completion_owner = LLMCompletionOwner::worker_turn(ctx.key());
    attach_completion_owner(&mut input.completion_request, &completion_owner);
    exclude_reserved_control_tool_schemas(&mut input.completion_request);
    let allowed_tools = allowed_tool_names(&input.completion_request);

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
        let call = crate::restate_identity::replay_safe_request(
            ctx.service_client::<LLMGatewayClient>()
                .complete(Json::from(input.completion_request))
                .idempotency_key(completion_idempotency_key(
                    ctx.invocation_id(),
                    LLMCompletionAction::WorkerModel {
                        turn: input.model_turn,
                    },
                )),
        )
        .call();
        match cancel_and_join_child_call(
            ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE),
            call,
        )
        .await?
        {
            ChildInvocationOutcome::Cancelled(reason) => {
                return Ok(WorkerIterationOutcome::Cancelled(reason));
            }
            ChildInvocationOutcome::Completed(response) => {
                let response = response.into_inner();
                if response.stop_reason == StopReason::Cancelled {
                    let reason = driver_progress::cancel_requested(ctx)
                        .await?
                        .unwrap_or_else(|| "worker turn cancelled".to_string());
                    return Ok(WorkerIterationOutcome::Cancelled(reason));
                }
                response
            }
        }
    };
    record_turn_llm_call_duration(llm_started.elapsed());
    let (response, verification_annotated) =
        annotate_unresolved_verification(&response, &*input.turn_evidence);

    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
            .record_response(Json::from(WorkerTurnResponseRecord {
                turn_id: input.request.turn_id.clone(),
                response: response.clone(),
            })),
    )
    .call()
    .await?;

    if verification_annotated {
        let outcome = CoreTurnOutcome::Idle;
        moa_core::coordination_counters::record_worker_vo_call();
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
                .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
                    turn_id: input.request.turn_id.clone(),
                    outcome,
                })),
        )
        .call()
        .await?;
        return Ok(WorkerIterationOutcome::Core(outcome));
    }

    for (index, tool_call) in response_tool_calls(&response).into_iter().enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
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
                    workflow.event_appender(),
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
            generation: input.request.generation,
            model_turn: input.model_turn,
            worker_id: &input.request.worker_id,
            meta: &input.meta,
            identity: &input.request.identity,
            session_id: input.parent_session,
            active_canary: input.active_canary.as_deref(),
            trusted_sandbox_manifest: input.request.trusted_sandbox_manifest.as_ref(),
            selected_skills: &selected_skills,
            tool_catalog_pin: &input.tool_catalog_pin,
            disabled_capabilities: &mut *input.disabled_capabilities,
        };
        match handle_tool_call(
            workflow,
            ctx,
            tool_context,
            &allowed_tools,
            index,
            tool_call,
            &mut *input.turn_evidence,
        )
        .await?
        {
            WorkerToolCallDisposition::SecurityHalt => {
                return Ok(WorkerIterationOutcome::SecurityHalt);
            }
            WorkerToolCallDisposition::Cancelled(reason) => {
                return Ok(WorkerIterationOutcome::Cancelled(reason));
            }
            WorkerToolCallDisposition::Continue | WorkerToolCallDisposition::SecurityNeedsInput => {
            }
        }
    }

    let outcome = turn_outcome_for_response(&response);
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(input.request.worker_id.clone())
            .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
                turn_id: input.request.turn_id.clone(),
                outcome,
            })),
    )
    .call()
    .await?;
    Ok(WorkerIterationOutcome::Core(outcome))
}

/// Refreshes the child's telemetry-plane heartbeat at the progress cadence.
///
/// The timestamp is journaled via `durable_utc_now` so it stays replay-stable, then
/// durably delivered to the `Worker` VO. This is VO state only (no event
/// per tick); the watchdog and `progress_summary` read it to detect a stuck child.
async fn record_worker_heartbeat(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx, "worker_heartbeat").await?;
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(worker_id.to_string())
            .record_heartbeat(Json::from(now)),
    )
    .call()
    .await?;
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
    let Some(segment) = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(parent_session)),
    )
    .call()
    .await?
    .into_inner()
    .map(|segment| segment.active_view()) else {
        return Ok(Vec::new());
    };
    driver_segments::insert_active_segment_metadata(request, &segment);
    Ok(segment.skills_activated)
}

struct WorkerToolContext<'a> {
    turn_id: &'a str,
    /// Worker generation that admitted this turn, recorded on any action review it queues.
    generation: u64,
    model_turn: usize,
    worker_id: &'a str,
    meta: &'a SessionMeta,
    identity: &'a moa_core::traits::Identity,
    session_id: SessionId,
    active_canary: Option<&'a str>,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    /// Skills the root turn activated on the active segment, used to detect which a worker
    /// tool call engaged so worker skill use is credited to the root segment.
    selected_skills: &'a [String],
    tool_catalog_pin: &'a moa_hands::ToolCatalogPin,
    disabled_capabilities: &'a mut BTreeMap<String, moa_core::types::security::ToolCapabilityId>,
}

async fn handle_tool_call(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    mut tool_context: WorkerToolContext<'_>,
    allowed_tools: &BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<WorkerToolCallDisposition, HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let worker_id = tool_context.worker_id;
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let selected_skills = tool_context.selected_skills;
    let tool_id = stable_worker_tool_call_id(
        session_id,
        worker_id,
        tool_context.turn_id,
        tool_context.generation,
        tool_context.model_turn,
        index,
        tool_call,
    );

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
                generation: tool_context.generation,
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

    if let Some(disabled_capability) = disabled_capability_for_tool(
        tool_context.disabled_capabilities,
        &tool_call.invocation.name,
    )
    .cloned()
    {
        refuse_disabled_worker_capability(
            workflow,
            ctx,
            &tool_context,
            &disabled_capability,
            tool_id,
            tool_call,
            turn_evidence,
        )
        .await?;
        return Ok(WorkerToolCallDisposition::Continue);
    }

    let mut disposition = WorkerToolCallDisposition::Continue;
    let outcome = invoke_governed_tool(
        ctx,
        GovernedInvocationRequest {
            session: meta,
            identity: tool_context.identity,
            session_id,
            tool_id,
            tool_call,
            allowed_tools,
            expected_tool_contract_revision: tool_context
                .tool_catalog_pin
                .contract_revision(&tool_call.invocation.name),
            active_canary: tool_context.active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::Worker {
                worker_id,
                turn_id: tool_context.turn_id,
                generation: tool_context.generation,
            },
            capability_provenance: None,
            capability_policy_context: None,
            resource_budget: moa_core::types::resource::ResourceBudget::UNBOUNDED,
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
            disposition =
                apply_worker_security_assessment(workflow, ctx, &mut tool_context, &result).await?;
            turn_evidence.record_tool_result(&result.invocation, &result.output.safe_output);
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
        GovernedInvocationOutcome::ExternalJob { .. }
        | GovernedInvocationOutcome::UnknownOutcome { .. }
        | GovernedInvocationOutcome::NotDispatched { .. } => {
            return Err(TerminalError::new(
                "worker-origin governed invocation returned an execution-only outcome",
            )
            .into());
        }
    }
    if disposition == WorkerToolCallDisposition::SecurityNeedsInput {
        // Reuse the existing request_input round-trip rather than inventing a
        // second suspension mechanism: it already emits one `NeedsInput` signal,
        // registers the awakeable on the Worker VO before emitting so a reply can
        // never race ahead of it, and clears the mapping on timeout.
        if let ChildInputWaitOutcome::Cancelled(reason) = request_input_from_parent(
            workflow,
            ctx,
            ChildInputRequestOwner {
                worker_id,
                turn_id: tool_context.turn_id,
                generation: tool_context.generation,
                parent_session: session_id,
            },
            &moa_core::types::worker::commands::RequestInputInput {
                question: WORKER_SECURITY_INPUT_QUESTION.to_string(),
                audience: moa_core::types::worker::state::InputAudience::User,
            },
        )
        .await?
        {
            return Ok(WorkerToolCallDisposition::Cancelled(reason));
        }
    }
    Ok(disposition)
}

/// Resolves a disabled registered tool name back to its typed capability identity.
fn disabled_capability_for_tool<'a>(
    disabled_capabilities: &'a BTreeMap<String, moa_core::types::security::ToolCapabilityId>,
    tool_name: &str,
) -> Option<&'a moa_core::types::security::ToolCapabilityId> {
    disabled_capabilities.get(tool_name)
}

/// Records a fixed refusal without dispatching a worker capability disabled by its circuit.
async fn refuse_disabled_worker_capability(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: &WorkerToolContext<'_>,
    capability: &moa_core::types::security::ToolCapabilityId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    append_tool_call_event(
        workflow.event_appender(),
        ctx,
        tool_context.session_id,
        tool_id,
        tool_call,
    )
    .await?;
    let secured = moa_security::classify_tool_output(
        &ToolOutput::error(WORKER_DISABLED_CAPABILITY_MESSAGE, Duration::ZERO),
        moa_security::OutputClassification {
            capability,
            active_canary: None,
        },
    );
    append_tool_result_event(
        workflow.event_appender(),
        ctx,
        tool_context.session_id,
        tool_id,
        &tool_call.invocation,
        &secured,
    )
    .await?;
    record_denied_tool(
        ctx,
        tool_context.turn_id,
        tool_context.worker_id,
        tool_id,
        &tool_call.invocation,
        &secured,
    )
    .await?;
    turn_evidence.record_tool_result(&tool_call.invocation, &secured.safe_output);
    Ok(())
}

/// Fixed model-facing refusal for a capability disabled by the worker circuit.
const WORKER_DISABLED_CAPABILITY_MESSAGE: &str = "This tool capability is disabled for the current worker turn because its prior output triggered the security circuit.";

/// Fixed question asked when the circuit suspends a worker turn.
///
/// Fixed, not derived: the output that triggered this is precisely what MOA has
/// decided it cannot trust, so quoting it into a user-facing question would
/// forward the attack to the human.
const WORKER_SECURITY_INPUT_QUESTION: &str = "A tool this worker used returned output that MOA classified as a possible \
     prompt-injection attempt, and that capability is now disabled for this turn. \
     How would you like the worker to proceed?";

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
    append_tool_call_event(
        workflow.event_appender(),
        ctx,
        session_id,
        tool_id,
        tool_call,
    )
    .await?;
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

    // Worker-authored control-plane refusals are classified on the same path as
    // provider output: the refusal text embeds model-authored tool arguments.
    let secured = secured_worker_output(&invocation, output);
    append_tool_result_event(
        workflow.event_appender(),
        ctx,
        session_id,
        tool_id,
        &invocation,
        &secured,
    )
    .await?;
    record_denied_tool(ctx, turn_id, worker_id, tool_id, &invocation, &secured).await?;
    turn_evidence.record_tool_result(&invocation, &secured.safe_output);
    if !secured.is_error() {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

/// One child-only report tool invocation routed inside the child's own turn loop.
struct ChildReportToolRequest<'a> {
    turn_id: &'a str,
    /// Worker admission generation that owns this turn, recorded on any input request
    /// it raises so a reply or clear can name exactly this owner.
    generation: u64,
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
) -> Result<WorkerToolCallDisposition, HandlerError> {
    let ChildReportToolRequest {
        turn_id,
        generation,
        worker_id,
        parent_session,
        tool_id,
        tool_call,
        report_tool,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(
        workflow.event_appender(),
        ctx,
        parent_session,
        tool_id,
        tool_call,
    )
    .await?;
    record_worker_heartbeat(ctx, worker_id).await?;
    let output = match report_tool {
        ChildReportTool::Report(input) => {
            report_to_parent(ctx, worker_id, parent_session, &input).await?
        }
        ChildReportTool::RequestInput(input) => {
            match request_input_from_parent(
                workflow,
                ctx,
                ChildInputRequestOwner {
                    worker_id,
                    turn_id,
                    generation,
                    parent_session,
                },
                &input,
            )
            .await?
            {
                ChildInputWaitOutcome::Output { output, .. } => *output,
                ChildInputWaitOutcome::Cancelled(reason) => {
                    return Ok(WorkerToolCallDisposition::Cancelled(reason));
                }
            }
        }
    };
    let secured = secured_worker_output(&invocation, output);
    append_tool_result_event(
        workflow.event_appender(),
        ctx,
        parent_session,
        tool_id,
        &invocation,
        &secured,
    )
    .await?;
    record_tool_result(ctx, turn_id, worker_id, tool_id, &invocation, &secured).await?;
    turn_evidence.record_tool_result(&invocation, &secured.safe_output);
    Ok(WorkerToolCallDisposition::Continue)
}

/// Emits a model-driven `Finding`/`Blocked` control-plane signal to the coordinator.
///
/// `signal_id`/`created_at` are journaled (`ctx.run`/`durable_utc_now`) for replay safety
/// and the cross-VO `record_child_signal` is awaited before the child reports success. A `Finding` records without
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
    moa_core::coordination_counters::record_session_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .record_child_signal(Json::from(signal)),
    )
    .call()
    .await?;
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
    owner: ChildInputRequestOwner<'_>,
    input: &RequestInputInput,
) -> Result<ChildInputWaitOutcome, HandlerError> {
    let ChildInputRequestOwner {
        worker_id,
        turn_id,
        generation,
        parent_session,
    } = owner;
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
    // The waiting workflow is this turn's own invocation (the workflow key is the turn
    // id), recorded so a clear retracts exactly this registration and not one a retry
    // of the same logical turn installed.
    let target = WorkerInputTarget {
        turn_id: turn_id.to_string(),
        generation,
        input_request_id: input_request_id.clone(),
    };
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(worker_id.to_string())
            .register_input_request(Json::from(WorkerPendingInput {
                turn_id: target.turn_id.clone(),
                generation,
                input_request_id: input_request_id.clone(),
                awakeable_id,
                waiting_workflow_id: turn_id.to_string(),
            })),
    )
    .call()
    .await?;

    // Emit the NeedsInput signal to the owning coordinator (arms a guarded resume if the
    // coordinator is idle) before waiting, so cancellation cannot leave an unrecorded target.
    let signal = build_needs_input_signal(
        worker_id,
        parent_session,
        signal_id,
        created_at,
        &target,
        input.audience,
        &input.question,
    );
    moa_core::coordination_counters::record_session_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<SessionClient>(parent_session.to_string())
            .record_child_signal(Json::from(signal)),
    )
    .call()
    .await?;

    let timeout_ms = workflow.session_limits.worker_input_timeout_ms;
    let output = restate_sdk::select! {
        answer = answer_future => {
            ChildInputWaitOutcome::Output {
                output: Box::new(ToolOutput::text(
                    format!("Input received: {}", answer?),
                    Duration::ZERO,
                )),
                clear_registration: false,
            }
        },
        reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
            ChildInputWaitOutcome::Cancelled(reason?)
        },
        _ = ctx.sleep(Duration::from_millis(timeout_ms)) => {
            ChildInputWaitOutcome::Output {
                output: Box::new(ToolOutput::text(
                    "No input was received in time. Proceed with your best judgment or report that you are blocked."
                        .to_string(),
                    Duration::ZERO,
                )),
                clear_registration: true,
            }
        }
    };
    let clear_registration = match &output {
        ChildInputWaitOutcome::Output {
            clear_registration, ..
        } => *clear_registration,
        ChildInputWaitOutcome::Cancelled(_) => true,
    };
    if clear_registration {
        moa_core::coordination_counters::record_worker_vo_call();
        crate::restate_identity::replay_safe_request(
            ctx.object_client::<WorkerClient>(worker_id.to_string())
                .clear_input_request(Json::from(WorkerClearInputRequest {
                    target,
                    waiting_workflow_id: turn_id.to_string(),
                })),
        )
        .call()
        .await?;
    }
    Ok(output)
}

enum ChildInputWaitOutcome {
    Output {
        output: Box<ToolOutput>,
        clear_registration: bool,
    },
    Cancelled(String),
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
        input_request: None,
    }
}

/// Exact owner of one child `request_input` round-trip.
///
/// The turn and its admission generation travel with the request so the Worker VO
/// mapping, the coordinator-advertised reply target, and every clear all name the
/// same owner instead of a bare request id.
struct ChildInputRequestOwner<'a> {
    worker_id: &'a str,
    turn_id: &'a str,
    generation: u64,
    parent_session: SessionId,
}

/// Builds the `NeedsInput` control-plane signal for a child `request_input` round-trip.
///
/// Kept pure so the carried owner coordinates, audience, and the resume-eligible
/// `IfIdle` policy are unit-testable. The caller journals `signal_id`/`created_at` and owns
/// the awakeable lifecycle.
fn build_needs_input_signal(
    worker_id: &str,
    parent_session: SessionId,
    signal_id: AgentSignalId,
    created_at: DateTime<Utc>,
    target: &WorkerInputTarget,
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
        input_request: Some(WorkerInputRequest {
            turn_id: target.turn_id.clone(),
            generation: target.generation,
            input_request_id: target.input_request_id.clone(),
            audience,
        }),
    }
}

async fn record_worker_budget_stop(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<String, HandlerError> {
    emit_tool_budget_exceeded(appender, ctx, parent_session, exhaustion).await?;
    let message = exhaustion.assistant_message();
    append_zero_cost_assistant_response(appender, ctx, parent_session, meta, message.clone())
        .await?;
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
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(request.worker_id.clone())
            .record_response(Json::from(WorkerTurnResponseRecord {
                turn_id: request.turn_id.clone(),
                response,
            })),
    )
    .call()
    .await?;
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(request.worker_id.clone())
            .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
                turn_id: request.turn_id.clone(),
                outcome: CoreTurnOutcome::Idle,
            })),
    )
    .call()
    .await?;
    Ok(message)
}

async fn record_worker_turn_cap_stop(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    request: &RunWorkerTurnRequest,
    meta: &SessionMeta,
    parent_session: SessionId,
    max_turns: usize,
) -> Result<String, HandlerError> {
    record_session_error("turn_cap");
    append_session_event(
        appender,
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
    append_zero_cost_assistant_response(appender, ctx, parent_session, meta, message.clone())
        .await?;
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
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(request.worker_id.clone())
            .record_response(Json::from(WorkerTurnResponseRecord {
                turn_id: request.turn_id.clone(),
                response,
            })),
    )
    .call()
    .await?;
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(request.worker_id.clone())
            .apply_turn_outcome(Json::from(WorkerTurnOutcomeRecord {
                turn_id: request.turn_id.clone(),
                outcome: CoreTurnOutcome::Idle,
            })),
    )
    .call()
    .await?;
    Ok(message)
}

/// Scores one classified worker tool output against that worker's own circuit.
///
/// The Worker virtual object owns the read-score-write step, so it is atomic
/// against concurrent results in the same worker turn. A worker's circuit facts
/// are deliberately neutral in the shared Session history: the worker's own
/// signals and turn outcome are what suspend or terminate it, so a child's
/// transition must not look like pending root work to the session scheduler.
async fn apply_worker_security_assessment(
    workflow: &WorkerTurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tool_context: &mut WorkerToolContext<'_>,
    result: &crate::tool_invocation::governed::GovernedInvocationResult,
) -> Result<WorkerToolCallDisposition, HandlerError> {
    if result.output.assessment.is_safe() {
        return Ok(WorkerToolCallDisposition::Continue);
    }
    let owner = moa_core::types::security::SecurityCircuitOwner::Worker {
        worker_id: tool_context.worker_id.to_string(),
        turn_id: tool_context.turn_id.to_string(),
        generation: tool_context.generation,
    };
    // Journaled BEFORE the circuit moves; see the coordinator path for why the
    // ordering matters on a crash between apply and journal.
    let occurred_at = ctx
        .run(|| async move { Ok(Json::from(chrono::Utc::now())) })
        .name("worker_prompt_injection_transition_timestamp")
        .await?
        .into_inner();
    moa_core::coordination_counters::record_worker_vo_call();
    let applied = crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(tool_context.worker_id.to_string())
            .apply_security_assessment(Json::from(
                moa_wire::turn::ApplySecurityAssessmentRequest {
                    owner,
                    allow_superseded_owner_noop: false,
                    capability: result.output.capability.clone(),
                    tool_call_id: result.tool_id,
                    assessment: result.output.assessment.clone(),
                },
            )),
    )
    .call()
    .await?
    .into_inner();

    if !applied.stage.permits_dispatch() {
        tool_context.disabled_capabilities.insert(
            result.invocation.name.clone(),
            result.output.capability.clone(),
        );
    }
    let disposition = match applied.stage {
        moa_core::types::security::SecurityCircuitStage::Halted => {
            WorkerToolCallDisposition::SecurityHalt
        }
        moa_core::types::security::SecurityCircuitStage::SuspendedForInput => {
            WorkerToolCallDisposition::SecurityNeedsInput
        }
        _ => WorkerToolCallDisposition::Continue,
    };
    let Some(transition) = applied.transition else {
        return Ok(disposition);
    };
    let dedupe_key = transition.key.clone();
    append_session_event_with_dedupe_key(
        workflow.event_appender(),
        ctx,
        tool_context.session_id,
        moa_core::events::Event::PromptInjectionCircuitTransition {
            transition: transition.clone(),
            signals: result.output.assessment.signals.clone(),
            redacted_spans: result.output.assessment.redacted_spans,
            deduplicated_carriers: result.output.assessment.deduplicated_carriers,
        },
        dedupe_key,
    )
    .await?;

    crate::restate_identity::replay_safe_request(
        ctx.service_client::<crate::services::security_events::SecurityEventsClient>()
            .record_circuit_transition(Json::from(
                crate::services::security_events::RecordCircuitTransitionRequest {
                    tenant_id: tool_context.meta.tenant_id,
                    session_id: tool_context.session_id,
                    transition,
                    signals: result.output.assessment.signals.clone(),
                    occurred_at,
                },
            )),
    )
    .call()
    .await?;
    Ok(disposition)
}

/// Whether one dispatched worker tool call left the worker turn able to continue.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerToolCallDisposition {
    /// The worker turn may keep running.
    Continue,
    /// The circuit halted this worker; the turn must terminate.
    SecurityHalt,
    /// The circuit suspended this worker pending the user's answer.
    SecurityNeedsInput,
    /// Cooperative cancellation stopped an attached child wait.
    Cancelled(String),
}

/// Fixed message for a worker turn the security circuit halted.
const WORKER_SECURITY_CIRCUIT_HALT_MESSAGE: &str = "worker turn stopped: a tool returned output classified as a prompt-injection or \
     restricted-material result";

/// Classifies one worker control-plane tool output.
///
/// These are MOA-authored refusals and report acknowledgements, but they embed
/// model-authored invocation text, so they run through the same detector rather
/// than being trusted for being ours.
fn secured_worker_output(
    invocation: &ToolInvocation,
    raw: moa_core::types::tools::ToolOutput,
) -> moa_core::types::tools::SecuredToolOutput {
    moa_security::classify_tool_output(
        &raw,
        moa_security::OutputClassification {
            capability: &moa_core::types::security::ToolCapabilityId::builtin(&invocation.name),
            active_canary: None,
        },
    )
}

async fn record_tool_result(
    ctx: &WorkflowContext<'_>,
    turn_id: &str,
    worker_id: &str,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &moa_core::types::tools::SecuredToolOutput,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(worker_id.to_string())
            .record_tool_result(Json::from(WorkerToolRecord {
                turn_id: Some(turn_id.to_string()),
                tool_id,
                invocation: invocation.clone(),
                output: output.clone(),
            })),
    )
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
    output: &moa_core::types::tools::SecuredToolOutput,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(worker_id.to_string())
            .record_denied_tool(Json::from(WorkerToolRecord {
                turn_id: Some(turn_id.to_string()),
                tool_id,
                invocation: invocation.clone(),
                output: output.clone(),
            })),
    )
    .call()
    .await?;
    Ok(())
}

async fn notify_worker_of_outcome(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    outcome: &TurnOutcome,
) -> Result<(), HandlerError> {
    moa_core::coordination_counters::record_worker_vo_call();
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<WorkerClient>(worker_id.to_string())
            .record_turn_outcome(Json::from(outcome.clone())),
    )
    .call()
    .await?;
    Ok(())
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
    use std::collections::BTreeMap;

    use chrono::Utc;
    use moa_core::{
        types::identifiers::AgentSignalId, types::identifiers::SessionId,
        types::security::ToolCapabilityId, types::worker::commands::ChildReportKind,
        types::worker::commands::ReportToParentInput, types::worker::signals::ChildSignalKind,
        types::worker::signals::ParentResumePolicy, types::worker::signals::SignalSeverity,
        types::worker::state::InputAudience, types::worker::state::WorkerInputRequest,
        types::worker::state::WorkerInputTarget,
    };
    use moa_wire::turn::{TurnOutcome, TurnOutcomeKind};

    use super::{
        build_child_report_signal, build_needs_input_signal, disabled_capability_for_tool,
        enforce_worker_user_message_origin,
    };

    #[test]
    fn disabled_mcp_capability_matches_its_registered_tool_name() {
        // Pins: MCP circuit identity uses server plus remote tool, while the model
        // emits the server-qualified registered name. The typed disabled map must
        // retain both without flattening either into a collision-prone string key.
        let capability = ToolCapabilityId::mcp("search", "query");
        let mut disabled = BTreeMap::new();
        disabled.insert("mcp__6_search__query".to_string(), capability.clone());

        assert_eq!(
            disabled_capability_for_tool(&disabled, "mcp__6_search__query"),
            Some(&capability)
        );
        assert_eq!(disabled_capability_for_tool(&disabled, "query"), None);
    }

    #[test]
    fn worker_accepted_run_outcome_fails_user_message_origin_invariant() {
        // Pins: worker turns cannot turn any classifier/planner drift into detached Run admission.
        let run_uid = uuid::Uuid::new_v4();
        let outcome = enforce_worker_user_message_origin(
            "worker-turn",
            TurnOutcome {
                turn_id: "upstream-turn".to_string(),
                kind: TurnOutcomeKind::Accepted {
                    execution_run_uid: run_uid,
                },
                message: "accepted".to_string(),
            },
        );
        assert_eq!(outcome.turn_id, "worker-turn");
        assert_eq!(outcome.kind, TurnOutcomeKind::Failed);
        assert_eq!(outcome.message, "run_requires_user_message_origin");
    }

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
        assert!(signal.input_request.is_none());
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
    fn request_input_builds_needs_input_signal_with_exact_owner_and_audience() {
        // Pins: request_input builds a NeedsInput/IfIdle signal carrying the raising
        // turn, its generation, the request id, and the audience — the exact coordinates
        // the coordinator session advertises as a user-addressable reply target.
        let signal = build_needs_input_signal(
            "parent-1-child-1",
            SessionId::new(),
            AgentSignalId::new(),
            Utc::now(),
            &WorkerInputTarget {
                turn_id: "worker-turn-4".to_string(),
                generation: 6,
                input_request_id: "req-42".to_string(),
            },
            InputAudience::User,
            "Which staging cluster should I deploy to?",
        );
        assert_eq!(signal.kind, ChildSignalKind::NeedsInput);
        assert_eq!(signal.resume_policy, ParentResumePolicy::IfIdle);
        assert_eq!(
            signal.input_request,
            Some(WorkerInputRequest {
                turn_id: "worker-turn-4".to_string(),
                generation: 6,
                input_request_id: "req-42".to_string(),
                audience: InputAudience::User,
            })
        );
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
            &WorkerInputTarget {
                turn_id: "worker-turn-1".to_string(),
                generation: 1,
                input_request_id: "req-1".to_string(),
            },
            InputAudience::Coordinator,
            &question,
        );
        assert!(signal.summary.chars().count() <= 201);
        assert!(signal.summary.ends_with('…'));
        assert!(!signal.summary.contains("second line"));
    }
}
