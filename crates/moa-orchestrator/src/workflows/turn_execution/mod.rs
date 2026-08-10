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
mod event_queries;
mod experience;
mod guardrails;
pub mod implementation;
mod reporting;
mod request;
mod responses;
mod segments;
mod tools;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use async_trait::async_trait;
use moa_brain::execution_planning::{
    ExecutionPlanningRequest, ExecutionPlanningResultKind, ExecutionRoutingInput, plan_execution,
    route_execution,
};
use moa_brain::lineage::emit_generation_lineage;
use moa_brain::segment_assessment::AssessmentOverride;
use moa_core::{
    coordination_counters::CoordinationCounters,
    coordination_counters::scope_coordination_counters,
    events::Event,
    session_replay::TurnReplayCounters,
    session_replay::scope_turn_replay_counters,
    traits::LLMProvider,
    types::completion::DEFER_BRAIN_RESPONSE_METADATA_KEY,
    types::completion::{
        CompletionRequest, CompletionStream, SharedCompletionRequest, StopReason, TokenUsage,
    },
    types::context::ContextMessage,
    types::execution_planning::{
        AdmittedDurableUpgrade, DurableUpgradeSignal, DurableUpgradeTransitionError,
        ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, ExecutionRouteDecision,
        ExecutionRouteStage, ExecutionRouteUsage, ExecutionRoutingResult, ExecutionStrategy,
        durable_upgrade_transition, execution_planning_dedupe_key,
    },
    types::identifiers::{ModelId, SessionId},
    types::model::ModelCapabilities,
    types::segment_assessment::AssessmentPhase,
    types::session::SessionMeta,
    types::session::TurnOutcome as CoreTurnOutcome,
};
use moa_execution::repository::{
    CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope, PlannerCallAuditWriteOutcome,
    RouteAuditWriteOutcome,
};
use moa_lineage_core::TurnId;
use moa_observability::restate_observability::{
    emit_turn_coordination_summary, emit_turn_latency_summary, emit_turn_replay_summary,
    llm_call_span,
};
use moa_observability::{
    TurnLatencyCounters, record_session_error, record_turn_latency, record_turn_llm_call_duration,
    scope_turn_latency_counters,
};
use moa_wire::turn::{
    RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress, TurnTrigger,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use self::event_queries::{
    load_available_skill_names, load_recent_target_events, load_session_meta,
};
use self::guardrails::{
    OutputGuardrailOutcome, evaluate_input_guardrail, visible_response_after_output_guardrail,
};
use self::implementation::TurnExecutionImpl;
use self::reporting::{
    create_turn_span, emit_turn_cap_exceeded, maybe_append_turn_metrics, record_response,
    record_selected_segment_skills, selected_skill_names, turn_cap_reached_message,
};
use self::request::{BuiltTurnRequest, build_request_inside_workflow};
use self::responses::{
    append_brain_response_from_completion, append_clarification_response, has_user_message_origin,
    ingest_deferred_session_turn, is_action_review_turn, is_execution_synthesis_turn,
    last_response_cutoff_before_seq, record_last_response_sequence,
};
use self::segments::{
    PostOutcomeAssessment, capture_current_active_segment_assessment, ensure_current_segment,
    run_post_outcome_assessment,
};
use self::tools::{
    FileReadTurnCache, RootToolContext, ToolDispatchOutcome, configure_durable_upgrade_tool_schema,
    dispatch_response_tool_calls,
};

use crate::restate_identity::with_identity_headers;
use crate::services::{
    execution::ExecutionClient,
    llm_gateway::{
        BoundedCompletionRequest, LLMCompletionAction, LLMCompletionOwner, LLMGatewayClient,
        attach_completion_owner, completion_idempotency_key,
    },
};
use crate::tool_invocation::governed::completion_tool_catalog_pin;
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification,
    ensure_delegation_tool_schemas, exclude_reserved_control_tool_schemas, response_tool_calls,
    turn_outcome_for_response,
};
use crate::turn_driver::{
    model_loop as driver_model_loop, progress as driver_progress, segments as driver_segments,
};
use crate::workflows::child_invocation::{ChildInvocationOutcome, cancel_and_join_child_call};
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::{
    TurnEventAppender, append_session_event, append_zero_cost_assistant_response,
    append_zero_cost_assistant_response_with_sequence, emit_tool_budget_exceeded,
    turn_outcome_kind_label,
};
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL, SUMMARY_WORKING};
use crate::workflows::turn_responsiveness::{
    ModelLoopClass, ToolBudgetExhausted, ToolBudgetState, recent_target_digest,
};

#[derive(Clone, Debug)]
struct BodyOutcome {
    kind: TurnOutcomeKind,
    message: String,
    post_outcome_assessment: Option<PostOutcomeAssessment>,
}

async fn cancelled_body_outcome(ctx: &WorkflowContext<'_>) -> Result<BodyOutcome, HandlerError> {
    // The gateway can observe the shared provider-cancellation fence immediately
    // before `request_cancel` resolves this workflow promise. Awaiting the promise
    // closes that race without allowing a cancelled completion to enter another
    // model-loop iteration.
    let reason = ctx
        .promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE)
        .await?;
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Cancelled,
        message: reason,
        post_outcome_assessment: None,
    })
}

#[derive(Clone, Copy)]
struct RunOnceContext<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    /// Session-facing workflow turn key, the identity an action review owner records.
    workflow_turn_id: &'a str,
    /// Session turn generation that admitted this turn.
    generation: u64,
    model_turn: usize,
    loop_class: ModelLoopClass,
    objective: &'a str,
    processing_required: bool,
    durable_upgrade_allowed: bool,
    execution_synthesis_instruction: Option<&'a str>,
    identity: &'a moa_core::traits::Identity,
    resource_budget: moa_core::types::resource::ResourceBudget,
}

#[derive(Clone, Copy)]
enum RestateExecutionModelAction {
    Routing,
    InitialPlanning,
}

struct RestateExecutionModelProvider<'a> {
    ctx: &'a WorkflowContext<'a>,
    budget: moa_core::types::resource::ResourceBudget,
    action: RestateExecutionModelAction,
    next_attempt: AtomicUsize,
}

impl<'a> RestateExecutionModelProvider<'a> {
    fn new(
        ctx: &'a WorkflowContext<'a>,
        budget: moa_core::types::resource::ResourceBudget,
        action: RestateExecutionModelAction,
    ) -> Self {
        Self {
            ctx,
            budget,
            action,
            next_attempt: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LLMProvider for RestateExecutionModelProvider<'_> {
    fn name(&self) -> &'static str {
        "restate-llm-gateway"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        request: SharedCompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        // Restate's JSON transport requires the owned durable DTO. This is the
        // explicit serialization boundary after in-process shared routing.
        let mut request = CompletionRequest::from_view(&request);
        // Routing and planning invoke the provider sequentially; a planner repair
        // consumes the next explicit attempt coordinate in the same workflow replay.
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        let action = match self.action {
            RestateExecutionModelAction::Routing => {
                LLMCompletionAction::ExecutionRouting { attempt }
            }
            RestateExecutionModelAction::InitialPlanning => {
                LLMCompletionAction::InitialPlanning { attempt }
            }
        };
        attach_completion_owner(&mut request, &LLMCompletionOwner::root_turn(self.ctx.key()));
        let call = crate::restate_identity::replay_safe_request(
            self.ctx
                .service_client::<LLMGatewayClient>()
                .complete_bounded(Json::from(BoundedCompletionRequest {
                    request,
                    budget: self.budget,
                }))
                .idempotency_key(completion_idempotency_key(self.ctx.invocation_id(), action)),
        )
        .call();
        let response = match cancel_and_join_child_call(
            self.ctx
                .promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE),
            call,
        )
        .await
        .map_err(|error| moa_core::error::MoaError::ProviderError(format!("{error:?}")))?
        {
            ChildInvocationOutcome::Completed(response) => response.into_inner(),
            ChildInvocationOutcome::Cancelled(_) => {
                return Err(moa_core::error::MoaError::Cancelled);
            }
        };
        if response.stop_reason == StopReason::Cancelled {
            return Err(moa_core::error::MoaError::Cancelled);
        }
        Ok(CompletionStream::from_response(response))
    }
}

fn per_model_call_budget(
    budget: moa_core::types::resource::ResourceBudget,
) -> moa_core::types::resource::ResourceBudget {
    let Some(remaining) = budget.remaining else {
        return budget;
    };
    let calls = remaining.model_calls.max(1);
    moa_core::types::resource::ResourceBudget::new(
        budget.deadline,
        Some(moa_core::types::resource::ResourceAmounts {
            cost_micro_usd: remaining.cost_micro_usd / calls,
            tokens: remaining.tokens / calls,
            turns: 0,
            model_calls: 1,
            tool_calls: 0,
        }),
    )
}

#[derive(Clone, Debug)]
enum TurnIterationOutcome {
    Core(CoreTurnOutcome),
    DurableUpgrade(DurableUpgradeSignal),
    DurableUpgradeUnsupported(String),
    ToolBudgetExceeded(ToolBudgetExhausted),
    /// The prompt-injection circuit halted this coordinator turn.
    SecurityHalt,
    /// The coordinator's bounded security-input wait expired.
    SecurityInputTimedOut,
}

struct DurableUpgradeGuard {
    has_root_user_origin: bool,
    initial_route: ExecutionRouteDecision,
    consumed: bool,
}

impl DurableUpgradeGuard {
    fn new(request: &RunTurnRequest, initial_route: &ExecutionRouteDecision) -> Self {
        Self {
            has_root_user_origin: has_user_message_origin(request),
            initial_route: initial_route.clone(),
            consumed: false,
        }
    }

    fn allows_tool_signal(&self) -> bool {
        self.has_root_user_origin
            && !self.consumed
            && self.initial_route.strategy() == Some(ExecutionStrategy::Inline)
    }

    fn consume(
        &mut self,
        originating_objective: &str,
        signal: DurableUpgradeSignal,
    ) -> Result<AdmittedDurableUpgrade, DurableUpgradeTransitionError> {
        let admitted = durable_upgrade_transition(
            originating_objective,
            &self.initial_route,
            self.has_root_user_origin,
            self.consumed,
            signal,
        )?;
        self.consumed = true;
        Ok(admitted)
    }
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

async fn execute_turn_inside_workflow(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: &RunTurnRequest,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<BodyOutcome, HandlerError> {
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        return Ok(BodyOutcome {
            kind: TurnOutcomeKind::Cancelled,
            message: reason,
            post_outcome_assessment: None,
        });
    }

    // The trigger and the typed continuation context must agree exactly. A mismatch
    // is a wiring defect, not something to infer around: an `ActionReview` turn with
    // no receipt has nothing to continue, and a receipt on any other trigger would
    // smuggle review state into an ordinary turn.
    request
        .action_review_continuation()
        .map_err(|error| TerminalError::new_with_code(409, error.to_string()))?;

    turn_progress::initialize(ctx).await?;
    turn_progress::enable_live_delivery(ctx);
    let appender = workflow.event_appender();

    let meta = load_session_meta(ctx, workflow.session_store.clone(), session_id).await?;
    let user_sequence_num = match request.trigger {
        TurnTrigger::UserMessage => {
            if let Some(outcome) = evaluate_input_guardrail(
                workflow,
                ctx,
                session_id,
                &meta,
                &request.user_message,
                request.resource_budget,
            )
            .await?
            {
                return Ok(outcome);
            }

            let user_event = append_session_event(
                appender,
                ctx,
                session_id,
                Event::UserMessage {
                    text: request.user_message.clone(),
                    attachments: request.attachments.clone(),
                },
            )
            .await?;
            let user_sequence_num = user_event.sequence_num;
            ctx.set(
                driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE,
                Json::from(user_sequence_num),
            );
            user_sequence_num
        }
        TurnTrigger::ChildSignal
        | TurnTrigger::WorkerResults
        | TurnTrigger::ExecutionSynthesis
        | TurnTrigger::ActionReview => {
            // System-triggered coordinator resume: the instruction was already recorded
            // as a durable control event and the history pipeline renders that event into
            // the prompt. So we deliberately do NOT append a fake `Event::UserMessage`,
            // skip the user-input guardrail, and leave the USER_MESSAGE_SEQUENCE anchor
            // unset — there is no human input on this turn.
            //
            // Recent `Event::WorkerSignalReceived` records are now rendered as
            // system-visible `<child_signal>` directives by the history pipeline
            // (`moa-brain` conversion), so a blocked or input-awaiting child is surfaced on
            // ANY coordinator turn — including a plain `UserMessage` turn — not just this
            // guarded resume path. A `NeedsInput` directive carries the child's
            // `input_request_id`, letting the coordinator answer via `provide_worker_input`
            // (e.g. on the user's reply turn for a `User`-audience request). Recency is
            // bounded by the existing compaction/history window, so addressed signals are not
            // re-surfaced indefinitely.
            tracing::info!(
                session_id = %request.session_id,
                turn_id = %request.turn_id,
                trigger = ?request.trigger,
                child_signal_id = ?request.child_signal_id,
                "TurnExecution seeding system-triggered coordinator resume turn"
            );
            0
        }
    };
    ctx.clear(driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE);
    ctx.clear(driver_progress::RootTurnStateKey::LAST_RESPONSE_SEQUENCE);

    let recent_target_events =
        load_recent_target_events(ctx, workflow.session_store.clone(), session_id).await?;
    let recent_target_digest = recent_target_digest(&recent_target_events, user_sequence_num);
    let execution_synthesis_turn = is_execution_synthesis_turn(request);
    // A review continuation answers work the session already routed. Re-running the
    // classifier would spend a model call to re-decide a settled question and could
    // route the continuation into planning or durable execution, which the exact
    // continuation matrix forbids.
    let action_review_turn = is_action_review_turn(request);
    let classifier_model = ModelId::new(
        workflow
            .config
            .models
            .auxiliary
            .clone()
            .unwrap_or_else(|| workflow.config.models.main.clone()),
    );
    let route_provider = RestateExecutionModelProvider::new(
        ctx,
        per_model_call_budget(request.resource_budget),
        RestateExecutionModelAction::Routing,
    );
    let route_result = if execution_synthesis_turn || action_review_turn {
        None
    } else {
        let available_skill_names =
            load_available_skill_names(ctx, workflow.session_store.pool().clone(), meta.tenant_id)
                .await?;
        let mut result = match route_execution(
            &route_provider,
            ExecutionRoutingInput {
                objective: &request.user_message,
                execution_template: request.execution_template.as_ref(),
                attachment_count: request.attachments.len(),
                recent_target_digest: &recent_target_digest,
                available_skill_names: &available_skill_names,
                classifier_model: &classifier_model,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(moa_core::error::MoaError::Cancelled) => {
                return cancelled_body_outcome(ctx).await;
            }
            Err(error) => {
                return Err(crate::workflows::errors::moa_error_to_handler_error(error));
            }
        };
        apply_route_cost(&mut result)?;
        Some(result)
    };
    let route = if execution_synthesis_turn {
        ExecutionRouteDecision::Respond {
            rationale: "This turn synthesizes the completed durable execution.".to_string(),
        }
    } else if action_review_turn {
        ExecutionRouteDecision::Respond {
            rationale: "This turn continues the owner after an action review resolved.".to_string(),
        }
    } else {
        route_result
            .as_ref()
            .ok_or_else(|| TerminalError::new("execution route result is missing"))?
            .decision
            .clone()
    };
    let durable_route = matches!(
        &route,
        ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Durable,
            ..
        }
    );
    if !has_user_message_origin(request) && (request.execution_template.is_some() || durable_route)
    {
        return Err(TerminalError::new_with_code(
            409,
            "durable_execution_requires_user_message_origin",
        )
        .into());
    }
    if request.trigger == TurnTrigger::UserMessage {
        let route_audit = route_audit_envelope(
            ctx,
            &meta,
            session_id,
            user_sequence_num,
            route_result
                .as_ref()
                .ok_or_else(|| TerminalError::new("user route evidence is missing"))?,
            ExecutionRouteStage::Initial,
        )
        .await?;
        persist_planning_audit(workflow, ctx, route_audit).await?;
    }

    match &route {
        ExecutionRouteDecision::NeedsInput { missing_inputs, .. } => {
            driver_progress::initialize_non_loop_progress(ctx, route.clone());
            let message =
                append_clarification_response(appender, ctx, session_id, &meta, missing_inputs)
                    .await?;
            return Ok(BodyOutcome {
                kind: TurnOutcomeKind::Completed,
                message,
                post_outcome_assessment: None,
            });
        }
        ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Durable,
            ..
        } => {
            if !request.resource_budget.is_unbounded() {
                return Err(TerminalError::new(
                    "a resource-bounded session turn cannot hand work to an unbounded durable execution",
                )
                .into());
            }
            driver_progress::initialize_non_loop_progress(ctx, route.clone());
            return execute_durable_admission(
                workflow,
                ctx,
                request,
                &meta,
                session_id,
                user_sequence_num,
                None,
            )
            .await;
        }
        ExecutionRouteDecision::Respond { .. }
        | ExecutionRouteDecision::Execute {
            strategy: ExecutionStrategy::Inline,
            ..
        } => {}
    }

    let session_limits = workflow.session_limits();
    let loop_plan = driver_model_loop::root_loop_plan(
        driver_model_loop::RootLoopPlanRequest {
            route: &route,
            request_max_turns: request.max_turns,
            resource_budget: request.resource_budget,
        },
        session_limits,
    )
    .ok_or_else(|| TerminalError::new("execution route did not select a model loop"))?;
    let loop_class = loop_plan.class;
    let mut tool_budget = loop_plan.tool_budget();
    // Once this turn spawns a worker, its model-loop cap escalates one-way to the higher
    // delegation cap so spawning, waiting on, and synthesizing workers fits in one turn.
    let mut turn_cap = loop_plan.turn_cap_escalation();
    driver_progress::initialize_loop_progress(
        ctx,
        loop_plan.route.clone(),
        loop_plan.max_turns,
        loop_plan.max_tool_calls,
    );
    let mut durable_upgrade_guard = DurableUpgradeGuard::new(request, &route);

    let mut last_summary = None;
    let mut turn_evidence = TurnEvidence::default();
    // Per-turn file_read memory serves repeated identical reads from context with a notice.
    let mut file_read_cache = FileReadTurnCache::default();
    // Tools whose capability the security circuit disabled for this turn. Rebuilt
    // deterministically from journaled apply-assessment responses, so a replay
    // reconstructs the same refusals.
    let mut disabled_tools = std::collections::BTreeSet::<String>::new();

    let mut turn_number: usize = 0;
    let final_max_turns = loop {
        turn_number += 1;
        let effective_max_turns = turn_cap.effective_max_turns();
        if turn_number > effective_max_turns {
            break effective_max_turns;
        }
        // Reset per iteration; run_once latches it on when this iteration spawns a worker.
        let mut delegated_this_turn = false;
        driver_progress::set_iteration(ctx, turn_number);
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            return Ok(BodyOutcome {
                kind: TurnOutcomeKind::Cancelled,
                message: reason,
                post_outcome_assessment: None,
            });
        }

        let span_meta = load_session_meta(ctx, workflow.session_store.clone(), session_id)
            .await
            .ok();
        let turn_root_span = create_turn_span(
            span_meta.as_ref(),
            Some(request.user_message.as_str()),
            turn_number,
            workflow.config.observability.environment.as_deref(),
        );
        let turn_counters = Arc::new(TurnReplayCounters::default());
        let turn_coordination_counters = Arc::new(CoordinationCounters::default());
        let turn_outcome = scope_coordination_counters(
            turn_coordination_counters.clone(),
            scope_turn_replay_counters(turn_counters.clone(), async {
                let turn_latency_counters =
                    Arc::new(TurnLatencyCounters::new(turn_root_span.clone()));
                let turn_started = Instant::now();
                let turn_result =
                    scope_turn_latency_counters(turn_latency_counters.clone(), async {
                        run_once_inside_workflow(
                            workflow,
                            ctx,
                            RunOnceContext {
                                session_id,
                                turn_id,
                                workflow_turn_id: request.turn_id.as_str(),
                                generation: request.generation,
                                model_turn: turn_number,
                                loop_class,
                                objective: &request.user_message,
                                processing_required: request_requires_processing(request.trigger),
                                durable_upgrade_allowed: durable_upgrade_guard.allows_tool_signal()
                                    && request.resource_budget.is_unbounded(),
                                execution_synthesis_instruction: execution_synthesis_turn
                                    .then_some(request.user_message.as_str()),
                                identity: &request.identity,
                                resource_budget: request.resource_budget,
                            },
                            &mut last_summary,
                            &mut turn_evidence,
                            &mut tool_budget,
                            &mut file_read_cache,
                            &mut disabled_tools,
                            &mut delegated_this_turn,
                        )
                        .instrument(turn_root_span.clone())
                        .await
                    })
                    .await;

                let turn_latency_snapshot = turn_latency_counters.snapshot();
                record_turn_latency(turn_started.elapsed());
                emit_turn_latency_summary(&turn_root_span, &turn_latency_snapshot);
                // Persist gated per-turn coordination/replay/latency for cost analysis and tests.
                // Snapshots are taken before this append so the telemetry record cannot count
                // itself.
                maybe_append_turn_metrics(
                    ctx,
                    session_id,
                    &turn_id.0.to_string(),
                    "coordinator",
                    &turn_coordination_counters.snapshot(),
                    &turn_counters.snapshot(),
                    turn_latency_snapshot.llm_call_ms(),
                    turn_latency_snapshot.tool_dispatch_ms(),
                    turn_latency_snapshot.event_persist_ms(),
                )
                .await?;
                turn_result
            }),
        )
        .await?;
        let turn_snapshot = turn_counters.snapshot();
        emit_turn_replay_summary(&turn_root_span, &turn_snapshot);
        let turn_coordination_snapshot = turn_coordination_counters.snapshot();
        emit_turn_coordination_summary(&turn_root_span, &turn_coordination_snapshot);

        // Latch the one-way delegation escalation and refresh the reported cap the first
        // time this turn spawns a worker, so later iterations use the higher cap.
        if delegated_this_turn && turn_cap.record_delegation() {
            driver_progress::set_max_turns(ctx, turn_cap.effective_max_turns());
        }

        match turn_outcome {
            TurnIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
            TurnIterationOutcome::DurableUpgrade(signal) => {
                let admitted = match durable_upgrade_guard.consume(&request.user_message, signal) {
                    Ok(admitted) => admitted,
                    Err(DurableUpgradeTransitionError::InvalidSignal(error)) => {
                        return durable_upgrade_unsupported_body(
                            appender,
                            ctx,
                            session_id,
                            &meta,
                            error.to_string(),
                        )
                        .await;
                    }
                    Err(error) => return Err(durable_upgrade_rejection(error)),
                };
                let route_audit = route_audit_envelope(
                    ctx,
                    &meta,
                    session_id,
                    user_sequence_num,
                    &admitted.routing,
                    ExecutionRouteStage::DurableUpgrade,
                )
                .await?;
                persist_planning_audit(workflow, ctx, route_audit).await?;
                driver_progress::set_execution_route(ctx, &admitted.routing.decision);
                return execute_durable_admission(
                    workflow,
                    ctx,
                    request,
                    &meta,
                    session_id,
                    user_sequence_num,
                    Some(admitted.signal),
                )
                .await;
            }
            TurnIterationOutcome::DurableUpgradeUnsupported(message) => {
                return durable_upgrade_unsupported_body(appender, ctx, session_id, &meta, message)
                    .await;
            }
            TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled) => {
                let post_outcome_assessment = capture_current_active_segment_assessment(
                    workflow,
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[AssessmentOverride::Cancelled],
                    last_response_cutoff_before_seq(ctx).await?,
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Cancelled,
                    message: last_summary
                        .take()
                        .unwrap_or_else(|| "turn cancelled by provider".to_string()),
                    post_outcome_assessment,
                });
            }
            TurnIterationOutcome::Core(CoreTurnOutcome::Idle) => {
                let post_outcome_assessment = capture_current_active_segment_assessment(
                    workflow,
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[],
                    last_response_cutoff_before_seq(ctx).await?,
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message: last_summary.unwrap_or_else(|| "idle".to_string()),
                    post_outcome_assessment,
                });
            }
            // A halted turn is a failure, not a completion: the coordinator's
            // catch-all boundary turns `Failed` into the canonical
            // `Event::TurnFailed { actor: Coordinator, .. }`, so the halt gets one
            // attributed failed-turn fact without a second writer for it here.
            // The message is fixed and carries nothing from the tool output.
            TurnIterationOutcome::SecurityHalt => {
                let post_outcome_assessment = capture_current_active_segment_assessment(
                    workflow,
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[],
                    last_response_cutoff_before_seq(ctx).await?,
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Failed,
                    message: SECURITY_CIRCUIT_HALT_MESSAGE.to_string(),
                    post_outcome_assessment,
                });
            }
            TurnIterationOutcome::SecurityInputTimedOut => {
                let post_outcome_assessment = capture_current_active_segment_assessment(
                    workflow,
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[],
                    last_response_cutoff_before_seq(ctx).await?,
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Failed,
                    message: last_summary.take().unwrap_or_else(|| {
                        "The turn stopped safely because required user input timed out.".to_string()
                    }),
                    post_outcome_assessment,
                });
            }
            TurnIterationOutcome::ToolBudgetExceeded(exhaustion) => {
                emit_tool_budget_exceeded(appender, ctx, session_id, &exhaustion).await?;
                let (message, sequence_num) = append_zero_cost_assistant_response_with_sequence(
                    appender,
                    ctx,
                    session_id,
                    &meta,
                    exhaustion.assistant_message(),
                )
                .await?;
                record_last_response_sequence(ctx, sequence_num);
                let post_outcome_assessment = capture_current_active_segment_assessment(
                    workflow,
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[AssessmentOverride::TurnCapExceeded],
                    Some(sequence_num.saturating_add(1)),
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message,
                    post_outcome_assessment,
                });
            }
        }
    };

    emit_turn_cap_exceeded(appender, ctx, session_id, final_max_turns).await?;
    let (message, sequence_num) = append_zero_cost_assistant_response_with_sequence(
        appender,
        ctx,
        session_id,
        &meta,
        turn_cap_reached_message(final_max_turns),
    )
    .await?;
    record_last_response_sequence(ctx, sequence_num);
    let post_outcome_assessment = capture_current_active_segment_assessment(
        workflow,
        ctx,
        session_id,
        AssessmentPhase::Final,
        &[AssessmentOverride::TurnCapExceeded],
        Some(sequence_num.saturating_add(1)),
    )
    .await?;
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Completed,
        message,
        post_outcome_assessment,
    })
}

fn durable_upgrade_rejection(rejection: DurableUpgradeTransitionError) -> HandlerError {
    let code = match &rejection {
        DurableUpgradeTransitionError::InvalidSignal(_) => 422,
        DurableUpgradeTransitionError::NotAuthorized
        | DurableUpgradeTransitionError::AlreadyConsumed
        | DurableUpgradeTransitionError::ObjectiveChanged => 409,
    };
    TerminalError::new_with_code(code, rejection.to_string()).into()
}

async fn durable_upgrade_unsupported_body(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    message: String,
) -> Result<BodyOutcome, HandlerError> {
    let message = append_zero_cost_assistant_response(
        appender,
        ctx,
        session_id,
        meta,
        format!("Durable execution upgrade is unsupported: {message}"),
    )
    .await?;
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Completed,
        message,
        post_outcome_assessment: None,
    })
}

fn apply_route_cost(result: &mut ExecutionRoutingResult) -> Result<(), HandlerError> {
    let Some(model) = result.provenance.provider_model.as_deref() else {
        return Ok(());
    };
    let usage = token_usage_from_route(result.provenance.usage)?;
    result.provenance.cost_microusd =
        crate::services::llm_gateway::compute_cost_micros(model, usage);
    Ok(())
}

fn token_usage_from_route(usage: ExecutionRouteUsage) -> Result<TokenUsage, HandlerError> {
    let convert = |value, field| {
        usize::try_from(value).map_err(|_| {
            HandlerError::from(TerminalError::new_with_code(
                422,
                format!("route {field} exceeds usize"),
            ))
        })
    };
    Ok(TokenUsage {
        input_tokens_uncached: convert(usage.input_tokens_uncached, "uncached input usage")?,
        input_tokens_cache_write: convert(
            usage.input_tokens_cache_write,
            "cache-write input usage",
        )?,
        input_tokens_cache_read: convert(usage.input_tokens_cache_read, "cache-read input usage")?,
        output_tokens: convert(usage.output_tokens, "output usage")?,
    })
}

async fn route_audit_envelope(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    session_id: SessionId,
    originating_sequence: u64,
    route: &ExecutionRoutingResult,
    stage: ExecutionRouteStage,
) -> Result<ExecutionPlanningAuditEnvelope, HandlerError> {
    let accepted_at_operation = match stage {
        ExecutionRouteStage::Initial => "execution_route_initial_accepted_at",
        ExecutionRouteStage::DurableUpgrade => "execution_route_durable_upgrade_accepted_at",
    };
    let accepted_at = durable_utc_now(ctx, accepted_at_operation).await?;
    Ok(ExecutionPlanningAuditEnvelope::route(
        meta.tenant_id,
        meta.contact.as_ref().map(|contact| contact.contact_id),
        session_id,
        originating_sequence,
        stage,
        &route.decision,
        route.provenance.clone(),
        accepted_at,
    ))
}

async fn persist_planning_audit(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    envelope: ExecutionPlanningAuditEnvelope,
) -> Result<(), HandlerError> {
    let scope = envelope.contact_id.map_or(
        ExecutionScope::Tenant {
            tenant_id: envelope.tenant_id,
        },
        |contact_id| ExecutionScope::Contact {
            tenant_id: envelope.tenant_id,
            contact_id,
        },
    );
    let durable_step_suffix = execution_planning_dedupe_key(&envelope)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?
        .strip_prefix("execution-planning:")
        .unwrap_or("audit")
        .to_string();
    match &envelope.payload {
        ExecutionPlanningAuditPayload::Route { .. } => {
            let pool = workflow.session_store.pool().clone();
            let audit = envelope.clone();
            let result = ctx
                .run(|| async move {
                    ExecutionRepository::new(pool)
                        .write_route_audit(scope, &audit)
                        .await
                        .map(Json::from)
                        .map_err(execution_audit_error)
                })
                .name(format!("execution_route_audit_{durable_step_suffix}"))
                .await?
                .into_inner();
            if matches!(result, RouteAuditWriteOutcome::Conflict { .. }) {
                return Err(planning_audit_conflict());
            }
        }
        ExecutionPlanningAuditPayload::PlannerCall { .. } => {
            let pool = workflow.session_store.pool().clone();
            let audit = envelope.clone();
            let result = ctx
                .run(|| async move {
                    ExecutionRepository::new(pool)
                        .write_planner_call_audit(scope, &audit)
                        .await
                        .map(Json::from)
                        .map_err(execution_audit_error)
                })
                .name(format!("execution_planner_audit_{durable_step_suffix}"))
                .await?
                .into_inner();
            if matches!(result, PlannerCallAuditWriteOutcome::Conflict { .. }) {
                return Err(planning_audit_conflict());
            }
        }
        ExecutionPlanningAuditPayload::Compile { .. } => {
            let pool = workflow.session_store.pool().clone();
            let audit = envelope;
            let result = ctx
                .run(|| async move {
                    ExecutionRepository::new(pool)
                        .write_compile_audit(scope, &audit)
                        .await
                        .map(Json::from)
                        .map_err(execution_audit_error)
                })
                .name(format!("execution_compile_audit_{durable_step_suffix}"))
                .await?
                .into_inner();
            if matches!(result, CompileAuditWriteOutcome::Conflict { .. }) {
                return Err(planning_audit_conflict());
            }
        }
    }
    Ok(())
}

fn planning_audit_conflict() -> HandlerError {
    TerminalError::new_with_code(
        409,
        "execution planning audit conflicts with first persisted evidence",
    )
    .into()
}

fn execution_audit_error(error: moa_execution::Error) -> HandlerError {
    TerminalError::new(format!("execution planning audit failed: {error}")).into()
}

/// Fixed user-safe reply for a planner provider/transport failure.
///
/// Deliberately opaque: the raw provider detail is recorded only in the durable
/// error event and never surfaced to the user.
/// Fixed user-facing message for a turn the security circuit halted.
///
/// Deliberately says nothing about what the output contained: the whole point of
/// the halt is that the output was untrustworthy, so quoting it into the reply
/// would hand the attacker the channel the circuit just closed.
const SECURITY_CIRCUIT_HALT_MESSAGE: &str = "This turn was stopped because a tool returned output that MOA classified as a \
     prompt-injection or restricted-material result. No further tool calls were made.";

const PLANNING_PROVIDER_FAILURE_USER_MESSAGE: &str =
    "I hit an internal error while planning this work. Please try again.";

/// Durable outcome for a planner provider/transport failure.
struct PlanningProviderFailure {
    /// Operator-facing durable error carrying the raw provider detail.
    error_event: Event,
    /// Bounded, user-safe assistant reply.
    user_message: String,
    /// Terminal turn status for the failure.
    outcome: TurnOutcomeKind,
}

/// Separates a planner provider failure into an operator-facing durable error and a
/// bounded user-safe reply.
///
/// The raw provider `detail` flows only into the recoverable [`Event::Error`]; the user
/// reply is the fixed [`PLANNING_PROVIDER_FAILURE_USER_MESSAGE`]. The turn is reported
/// [`TurnOutcomeKind::Failed`] to match how the inline model loop surfaces a bubbled
/// provider failure (see `implementation.rs`, which maps a handler error to
/// `TurnOutcomeKind::Failed`), because an infrastructure error is not a completed turn.
fn planning_provider_failure_outcome(detail: &str) -> PlanningProviderFailure {
    PlanningProviderFailure {
        error_event: Event::Error {
            message: format!("execution planning provider failure: {detail}"),
            recoverable: true,
        },
        user_message: PLANNING_PROVIDER_FAILURE_USER_MESSAGE.to_string(),
        outcome: TurnOutcomeKind::Failed,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_durable_admission(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    request: &RunTurnRequest,
    meta: &SessionMeta,
    session_id: SessionId,
    originating_user_sequence_num: u64,
    durable_upgrade: Option<DurableUpgradeSignal>,
) -> Result<BodyOutcome, HandlerError> {
    if !has_user_message_origin(request) {
        return Err(TerminalError::new_with_code(
            409,
            "durable_execution_requires_user_message_origin",
        )
        .into());
    }
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    let planning_call = ctx
        .service_client::<ExecutionClient>()
        .planning_context(Json::from(
            moa_execution::wire::ExecutionPlanningContextRequest {
                tenant_id: meta.tenant_id,
                contact_id,
                session_id,
                originating_user_sequence_num,
                requested_template: request
                    .execution_template
                    .as_ref()
                    .map(|invocation| invocation.template.clone()),
            },
        ));
    let planning_context = with_identity_headers(planning_call, &request.identity)
        .call()
        .await?
        .into_inner();

    let planner_model = workflow
        .config
        .models
        .auxiliary
        .clone()
        .unwrap_or_else(|| workflow.config.models.main.clone());
    let planning_now = durable_utc_now(ctx, "execution_planning_now").await?;
    let provider = RestateExecutionModelProvider::new(
        ctx,
        per_model_call_budget(request.resource_budget),
        RestateExecutionModelAction::InitialPlanning,
    );
    let planned = match plan_execution(
        &provider,
        ExecutionPlanningRequest {
            objective: request.user_message.clone(),
            context: planning_context.snapshot.clone(),
            execution_template: request.execution_template.clone(),
            durable_upgrade,
            planner_model: ModelId::new(planner_model),
            config: workflow.config.execution.clone(),
            now: planning_now,
        },
    )
    .await
    {
        Ok(planned) => planned,
        Err(moa_core::error::MoaError::Cancelled) => {
            return cancelled_body_outcome(ctx).await;
        }
        Err(error) => {
            return Err(crate::workflows::errors::moa_error_to_handler_error(error));
        }
    };
    for audit in planned.audits {
        persist_planning_audit(workflow, ctx, audit).await?;
    }
    let appender = workflow.event_appender();
    let admitted = match planned.kind {
        ExecutionPlanningResultKind::Ready(admitted) => admitted,
        ExecutionPlanningResultKind::NeedsInput { message } => {
            let message =
                append_zero_cost_assistant_response(appender, ctx, session_id, meta, message)
                    .await?;
            return Ok(BodyOutcome {
                kind: TurnOutcomeKind::Completed,
                message,
                post_outcome_assessment: None,
            });
        }
        ExecutionPlanningResultKind::Unsupported { message } => {
            // Planner-authored verdict text is safe to surface directly.
            let message =
                append_zero_cost_assistant_response(appender, ctx, session_id, meta, message)
                    .await?;
            return Ok(BodyOutcome {
                kind: TurnOutcomeKind::Completed,
                message,
                post_outcome_assessment: None,
            });
        }
        ExecutionPlanningResultKind::ProviderFailure { message } => {
            // Infrastructure failure, not a semantic verdict: record the raw provider
            // detail in a durable recoverable error for operators, reply with a bounded
            // user-safe message (never the raw string), and fail the turn to match the
            // inline model loop.
            let failure = planning_provider_failure_outcome(&message);
            record_session_error("execution_planning_provider_failure");
            append_session_event(appender, ctx, session_id, failure.error_event).await?;
            let message = append_zero_cost_assistant_response(
                appender,
                ctx,
                session_id,
                meta,
                failure.user_message,
            )
            .await?;
            return Ok(BodyOutcome {
                kind: failure.outcome,
                message,
                post_outcome_assessment: None,
            });
        }
    };

    let start_call = ctx.service_client::<ExecutionClient>().start(Json::from(
        moa_execution::wire::ExecutionStartRequest {
            tenant_id: meta.tenant_id,
            contact_id,
            session_id,
            originating_user_sequence_num,
            planning_context_uid: planning_context.planning_context_uid,
            planning_context_hash: planning_context.planning_context_hash,
            idempotency_key: Some(format!("turn:{}", request.turn_id)),
            compiled: admitted.compiled,
            run_input: admitted.run_input,
            source_provenance: admitted.source_provenance,
        },
    ));
    let started = with_identity_headers(start_call, &request.identity)
        .call()
        .await?
        .into_inner();
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Accepted {
            execution_run_uid: started.run.run_uid,
        },
        message: "Execution accepted.".to_string(),
        post_outcome_assessment: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_once_inside_workflow(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    turn_context: RunOnceContext<'_>,
    last_summary: &mut Option<String>,
    turn_evidence: &mut TurnEvidence,
    tool_budget: &mut ToolBudgetState,
    file_read_cache: &mut FileReadTurnCache,
    disabled_tools: &mut std::collections::BTreeSet<String>,
    delegated_this_turn: &mut bool,
) -> Result<TurnIterationOutcome, HandlerError> {
    let session_id = turn_context.session_id;
    let turn_id = turn_context.turn_id;
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
    }

    driver_progress::set_phase(ctx, TurnPhase::Compiling);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        SUMMARY_WORKING,
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let Some(built_request) = build_request_inside_workflow(
        ctx,
        workflow.request_preparer.clone(),
        workflow.session_store.clone(),
        session_id,
        turn_id,
        turn_context.identity.clone(),
        turn_context.processing_required,
    )
    .await?
    else {
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Idle));
    };
    let BuiltTurnRequest {
        mut request,
        active_canary,
        trusted_sandbox_files,
        trusted_sandbox_manifest,
        citation_sources,
    } = built_request;
    if let Some(instruction) = turn_context.execution_synthesis_instruction {
        request.messages.push(ContextMessage::system(instruction));
    }
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
    }

    let meta = load_session_meta(ctx, workflow.session_store.clone(), session_id).await?;
    workflow
        .tool_router
        .set_trusted_sandbox_files(&meta, None, trusted_sandbox_files.clone())
        .await;
    let active_segment =
        ensure_current_segment(workflow, ctx, session_id, &meta, &mut request).await?;
    if let Some(segment) = active_segment.as_ref() {
        driver_segments::insert_active_segment_metadata(&mut request, segment);
        record_selected_segment_skills(ctx, session_id, &request.metadata).await?;
    }
    if turn_context.loop_class == ModelLoopClass::Respond {
        request.tools.clear();
    } else {
        if turn_context.resource_budget.is_unbounded() {
            ensure_delegation_tool_schemas(&mut request);
        }
        exclude_reserved_control_tool_schemas(&mut request);
        configure_durable_upgrade_tool_schema(&mut request, turn_context.durable_upgrade_allowed);
    }
    request.metadata.insert(
        DEFER_BRAIN_RESPONSE_METADATA_KEY.to_string(),
        serde_json::json!(true),
    );
    attach_completion_owner(&mut request, &LLMCompletionOwner::root_turn(ctx.key()));
    let allowed_tools = allowed_tool_names(&request);
    let tool_catalog_pin = completion_tool_catalog_pin(&request)?;
    let request_model = request
        .model
        .as_ref()
        .map(|model| model.as_str())
        .unwrap_or(meta.model.as_str())
        .to_string();

    driver_progress::set_phase(ctx, TurnPhase::Streaming);
    turn_progress::maybe_emit(
        ctx,
        session_id,
        SUMMARY_CALLING_MODEL,
        workflow.session_limits(),
        workflow.session_store.clone(),
        workflow.channel_adapters.as_ref(),
    )
    .await?;
    let span = llm_call_span(&meta);
    let llm_started = Instant::now();
    let response = {
        let _guard = span.enter();
        let call = crate::restate_identity::replay_safe_request(
            ctx.service_client::<LLMGatewayClient>()
                .complete_bounded(Json::from(BoundedCompletionRequest {
                    request: request.clone(),
                    budget: per_model_call_budget(turn_context.resource_budget),
                }))
                .idempotency_key(completion_idempotency_key(
                    ctx.invocation_id(),
                    LLMCompletionAction::RootModel {
                        turn: turn_context.model_turn,
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
                *last_summary = Some(reason);
                return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
            }
            ChildInvocationOutcome::Completed(response) => response.into_inner(),
        }
    };
    if response.stop_reason == StopReason::Cancelled {
        let reason = ctx
            .promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE)
            .await?;
        *last_summary = Some(reason);
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
    }
    let llm_call_duration = llm_started.elapsed();
    record_turn_llm_call_duration(llm_call_duration);
    let (visible_response, output_blocked) = match visible_response_after_output_guardrail(
        workflow,
        ctx,
        session_id,
        &meta,
        &response,
        turn_context.resource_budget,
        turn_context.model_turn,
    )
    .await?
    {
        OutputGuardrailOutcome::Completed(response, blocked) => (response, blocked),
        OutputGuardrailOutcome::Cancelled(reason) => {
            *last_summary = Some(reason);
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
        }
    };
    let (visible_response, verification_annotated) =
        annotate_unresolved_verification(&visible_response, turn_evidence);
    let response_usage = visible_response.token_usage();
    let response_cost_micros = crate::services::llm_gateway::compute_cost_micros(
        visible_response.model.as_str(),
        response_usage,
    );
    let response_event = if visible_response.text.trim().is_empty() {
        None
    } else {
        let response_event = append_brain_response_from_completion(
            workflow.event_appender(),
            ctx,
            session_id,
            &visible_response,
        )
        .await?;
        record_last_response_sequence(ctx, response_event.sequence_num);
        ingest_deferred_session_turn(ctx, session_id, &request, response_event.sequence_num)
            .await?;
        Some(response_event)
    };
    emit_generation_lineage(
        workflow.lineage.as_ref(),
        turn_id,
        &meta,
        "llm_gateway",
        &request_model,
        &visible_response,
        &citation_sources,
        response_cost_micros,
        llm_call_duration,
        &span,
        response_event.as_ref(),
    )
    .await;

    record_response(ctx, session_id, &visible_response, last_summary).await?;

    if output_blocked || verification_annotated {
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Idle));
    }

    let tool_calls = response_tool_calls(&visible_response);
    let selected_skills = selected_skill_names(&request.metadata);
    match dispatch_response_tool_calls(
        workflow,
        ctx,
        RootToolContext {
            meta: &meta,
            identity: turn_context.identity,
            session_id,
            turn_id: turn_context.workflow_turn_id,
            generation: turn_context.generation,
            active_canary: active_canary.as_deref(),
            tool_catalog_pin: &tool_catalog_pin,
            trusted_sandbox_manifest: trusted_sandbox_manifest.as_ref(),
            selected_skills: &selected_skills,
            objective: turn_context.objective,
            durable_upgrade_allowed: turn_context.durable_upgrade_allowed,
            resource_budget: turn_context.resource_budget,
            turn_evidence,
            file_read_cache,
            disabled_tools,
            delegated_worker: delegated_this_turn,
        },
        &allowed_tools,
        tool_budget,
        &tool_calls,
        last_summary,
    )
    .await?
    {
        ToolDispatchOutcome::Completed => {}
        ToolDispatchOutcome::DurableUpgrade(signal) => {
            return Ok(TurnIterationOutcome::DurableUpgrade(signal));
        }
        ToolDispatchOutcome::DurableUpgradeUnsupported(message) => {
            return Ok(TurnIterationOutcome::DurableUpgradeUnsupported(message));
        }
        ToolDispatchOutcome::Cancelled => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
        }
        ToolDispatchOutcome::ToolBudgetExceeded(exhaustion) => {
            return Ok(TurnIterationOutcome::ToolBudgetExceeded(exhaustion));
        }
        ToolDispatchOutcome::SecurityHalt => {
            return Ok(TurnIterationOutcome::SecurityHalt);
        }
        ToolDispatchOutcome::SecurityInputTimedOut => {
            return Ok(TurnIterationOutcome::SecurityInputTimedOut);
        }
    }

    Ok(TurnIterationOutcome::Core(turn_outcome_for_response(
        &visible_response,
    )))
}

fn request_requires_processing(trigger: TurnTrigger) -> bool {
    trigger != TurnTrigger::UserMessage
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

#[cfg(test)]
mod tests;
