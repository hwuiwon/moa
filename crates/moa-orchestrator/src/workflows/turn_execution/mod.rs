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
mod delegation;
mod event_queries;
mod experience;
mod guardrails;
pub mod implementation;
mod request;
mod segments;
mod tools;

use std::sync::Arc;
use std::time::Instant;

use moa_brain::lineage::emit_generation_lineage;
use moa_brain::pipeline::skills::{
    SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY, SELECTED_SKILL_NAMES_METADATA_KEY,
};
use moa_brain::segment_assessment::AssessmentOverride;
use moa_core::wire::session_store::{
    AppendEventRequest, RecordSegmentSkillActivationRequest, RecordSegmentTurnUsageRequest,
};
use moa_core::wire::turn::{
    RunTurnRequest, TurnComplexityClass, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
    TurnTrigger,
};
use moa_core::{
    coordination_counters::CoordinationCounters,
    coordination_counters::scope_coordination_counters, events::Event,
    session_replay::TurnReplayCounters, session_replay::scope_turn_replay_counters,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::DEFER_BRAIN_RESPONSE_METADATA_KEY, types::identifiers::SessionId,
    types::provider::ModelTier, types::segment_assessment::AssessmentPhase,
    types::session::SessionMeta, types::session::TurnOutcome as CoreTurnOutcome,
};
use moa_lineage_core::TurnId;
use moa_memory_ingest::{IngestionVOClient, ingestion_object_key};
use moa_observability::restate_observability::{
    emit_turn_coordination_summary, emit_turn_latency_summary, emit_turn_replay_summary,
    llm_call_span, session_turn_span,
};
use moa_observability::{
    TurnLatencyCounters, record_session_error, record_turn_latency, record_turn_llm_call_duration,
    scope_turn_latency_counters,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use self::delegation::{
    AutoDelegationContext, AutoDelegationFanInOutcome, AutoDelegationOutcome,
    maybe_fan_in_auto_delegation_results, maybe_schedule_auto_delegation,
    root_request_turn_cap_for_auto_delegation,
};
use self::event_queries::{
    brain_response_event_by_sequence, load_recent_target_events, load_session_meta,
};
use self::guardrails::{evaluate_input_guardrail, visible_response_after_output_guardrail};
use self::implementation::TurnExecutionImpl;
use self::request::{BuiltTurnRequest, build_request_inside_workflow};
use self::segments::{
    PostOutcomeAssessment, capture_current_active_segment_assessment, ensure_current_segment,
    run_post_outcome_assessment,
};
use self::tools::{RootToolContext, ToolDispatchOutcome, dispatch_response_tool_calls};

use crate::objects::session::SessionClient;
use crate::restate_identity::with_identity_headers;
use crate::services::{llm_gateway::LLMGatewayClient, session_store::RestateSessionStoreClient};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification,
    ensure_delegation_tool_schemas, ensure_procedure_tool_schemas, response_tool_calls,
    summarize_response_text, turn_outcome_for_response,
};
use crate::turn_driver::{
    model_loop as driver_model_loop, progress as driver_progress, segments as driver_segments,
};
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::{
    append_session_event, append_zero_cost_assistant_response,
    append_zero_cost_assistant_response_with_sequence, emit_tool_budget_exceeded,
    turn_outcome_kind_label,
};
use crate::workflows::turn_progress::{self, SUMMARY_CALLING_MODEL, SUMMARY_WORKING};
use crate::workflows::turn_responsiveness::{
    ToolBudgetExhausted, ToolBudgetState, has_recent_target as recent_events_have_target,
};

#[derive(Clone, Debug)]
struct BodyOutcome {
    kind: TurnOutcomeKind,
    message: String,
    post_outcome_assessment: Option<PostOutcomeAssessment>,
}

#[derive(Clone, Copy)]
struct RunOnceContext<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    identity: &'a moa_core::traits::Identity,
}

#[derive(Clone, Debug)]
enum TurnIterationOutcome {
    Core(CoreTurnOutcome),
    ToolBudgetExceeded(ToolBudgetExhausted),
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

    turn_progress::initialize(ctx).await?;
    turn_progress::enable_live_delivery(ctx);

    let meta = load_session_meta(ctx, workflow.session_store.clone(), session_id).await?;
    let user_sequence_num = match request.trigger {
        TurnTrigger::UserMessage => {
            if let Some(outcome) =
                evaluate_input_guardrail(workflow, ctx, session_id, &meta, &request.user_message)
                    .await?
            {
                return Ok(outcome);
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
            ctx.set(
                driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE,
                Json::from(user_sequence_num),
            );
            user_sequence_num
        }
        TurnTrigger::ChildSignal | TurnTrigger::WorkerResults => {
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
    let has_recent_target = recent_events_have_target(&recent_target_events, user_sequence_num);
    let session_limits = workflow.session_limits();
    let request_max_turns =
        root_request_turn_cap_for_auto_delegation(&request.user_message, request.max_turns);
    let loop_plan = driver_model_loop::root_loop_plan(
        driver_model_loop::RootLoopPlanRequest {
            user_text: &request.user_message,
            attachment_count: request.attachments.len(),
            request_max_turns,
            has_recent_target,
            available_tool_count: workflow.tool_schemas.len(),
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
    if matches!(
        loop_plan.complexity_class,
        TurnComplexityClass::Clarification
    ) {
        let message = append_clarification_response(ctx, session_id, &meta).await?;
        return Ok(BodyOutcome {
            kind: TurnOutcomeKind::Completed,
            message,
            post_outcome_assessment: None,
        });
    }

    let mut last_summary = None;
    let mut turn_evidence = TurnEvidence::default();

    for turn_number in 1..=max_turns {
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
                                identity: &request.identity,
                            },
                            &mut last_summary,
                            &mut turn_evidence,
                            &mut tool_budget,
                        )
                        .instrument(turn_root_span.clone())
                        .await
                    })
                    .await;

                let turn_latency_snapshot = turn_latency_counters.snapshot();
                record_turn_latency(turn_started.elapsed());
                emit_turn_latency_summary(
                    &turn_root_span,
                    turn_number as i64,
                    &turn_latency_snapshot,
                );
                // Persist the per-turn coordination/replay/latency summary (gated) so the
                // conversation-cost analyzer and deterministic coordination tests can read it
                // from the durable log. Snapshots are taken before this append, so it does not
                // count itself.
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
        emit_turn_replay_summary(&turn_root_span, turn_number as i64, &turn_snapshot);
        let turn_coordination_snapshot = turn_coordination_counters.snapshot();
        emit_turn_coordination_summary(&turn_root_span, &turn_coordination_snapshot);

        match turn_outcome {
            TurnIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
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
            TurnIterationOutcome::ToolBudgetExceeded(exhaustion) => {
                emit_tool_budget_exceeded(ctx, session_id, &exhaustion).await?;
                let (message, sequence_num) = append_zero_cost_assistant_response_with_sequence(
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
    }

    emit_turn_cap_exceeded(ctx, session_id, max_turns).await?;
    let (message, sequence_num) = append_zero_cost_assistant_response_with_sequence(
        ctx,
        session_id,
        &meta,
        format!(
            "MOA stopped because this turn reached the model-loop turn cap ({max_turns}). Narrow the scope or ask MOA to continue."
        ),
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

async fn append_clarification_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
) -> Result<String, HandlerError> {
    let text = "What should I change? Point me at the file, message, object, or output and the specific fix you want.".to_string();
    append_zero_cost_assistant_response(ctx, session_id, meta, text).await
}

async fn append_brain_response_from_completion(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    response: &CompletionResponse,
) -> Result<u64, HandlerError> {
    let usage = response.token_usage();
    let cost_cents =
        crate::services::llm_gateway::compute_cost_cents(response.model.as_str(), usage);
    append_session_event(
        ctx,
        session_id,
        Event::BrainResponse {
            text: response.text.clone(),
            thought_signature: response.thought_signature.clone(),
            model: response.model.clone(),
            model_tier: ModelTier::Main,
            input_tokens_uncached: usage.input_tokens_uncached,
            input_tokens_cache_write: usage.input_tokens_cache_write,
            input_tokens_cache_read: usage.input_tokens_cache_read,
            output_tokens: usage.output_tokens,
            cost_cents,
            duration_ms: response.duration_ms,
        },
    )
    .await
}

fn record_last_response_sequence(ctx: &WorkflowContext<'_>, sequence_num: u64) {
    ctx.set(
        driver_progress::RootTurnStateKey::LAST_RESPONSE_SEQUENCE,
        Json::from(sequence_num),
    );
}

async fn last_response_cutoff_before_seq(
    ctx: &WorkflowContext<'_>,
) -> Result<Option<u64>, HandlerError> {
    Ok(ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::LAST_RESPONSE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .map(|sequence_num| sequence_num.saturating_add(1)))
}

async fn ingest_deferred_session_turn(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    request: &CompletionRequest,
    response: &CompletionResponse,
    response_sequence_num: u64,
) -> Result<(), HandlerError> {
    let finalized_at = durable_utc_now(ctx, "workflow_utc_now").await?;
    if let Some(turn) = crate::services::llm_gateway::session_turn_from_completion_request(
        request,
        &response.text,
        session_id,
        response_sequence_num,
        finalized_at,
    ) {
        ctx.object_client::<IngestionVOClient>(ingestion_object_key(&turn))
            .ingest_turn(Json(turn))
            .send();
    }
    Ok(())
}

async fn run_once_inside_workflow(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    turn_context: RunOnceContext<'_>,
    last_summary: &mut Option<String>,
    turn_evidence: &mut TurnEvidence,
    tool_budget: &mut ToolBudgetState,
) -> Result<TurnIterationOutcome, HandlerError> {
    let session_id = turn_context.session_id;
    let turn_id = turn_context.turn_id;
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
    }

    let progress_turn_id = turn_id.0.to_string();
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
    let Some(built_request) =
        build_request_inside_workflow(ctx, workflow.session_store.clone(), session_id, turn_id)
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
    ensure_delegation_tool_schemas(&mut request);
    if turn_has_procedure_capable_skill(&request.metadata) {
        ensure_procedure_tool_schemas(&mut request);
    }
    request.metadata.insert(
        DEFER_BRAIN_RESPONSE_METADATA_KEY.to_string(),
        serde_json::json!(true),
    );
    let allowed_tools = allowed_tool_names(&request);
    let request_model = request
        .model
        .as_ref()
        .map(|model| model.as_str())
        .unwrap_or(meta.model.as_str())
        .to_string();

    match maybe_schedule_auto_delegation(
        workflow,
        ctx,
        AutoDelegationContext {
            meta: &meta,
            session_id,
            trusted_sandbox_manifest: trusted_sandbox_manifest.as_ref(),
            turn_evidence,
        },
        &request,
        &allowed_tools,
        tool_budget,
        last_summary,
    )
    .await?
    {
        AutoDelegationOutcome::Skipped => {}
        AutoDelegationOutcome::Scheduled => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Continue));
        }
        AutoDelegationOutcome::Cancelled => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
        }
        AutoDelegationOutcome::ToolBudgetExceeded(exhaustion) => {
            return Ok(TurnIterationOutcome::ToolBudgetExceeded(exhaustion));
        }
    }

    match maybe_fan_in_auto_delegation_results(
        workflow,
        ctx,
        session_id,
        &progress_turn_id,
        last_summary,
    )
    .await?
    {
        AutoDelegationFanInOutcome::Skipped => {}
        AutoDelegationFanInOutcome::Continue => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Continue));
        }
        AutoDelegationFanInOutcome::Cancelled => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
        }
    }

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
        restate_sdk::select! {
            reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
                let reason = reason?;
                *last_summary = Some(reason);
                return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
            },
            response = ctx
                .service_client::<LLMGatewayClient>()
                .complete(Json::from(request.clone()))
                .call() => {
                    response?.into_inner()
            }
        }
    };
    let llm_call_duration = llm_started.elapsed();
    record_turn_llm_call_duration(llm_call_duration);
    let (visible_response, output_blocked) =
        visible_response_after_output_guardrail(workflow, ctx, session_id, &meta, &response)
            .await?;
    let (visible_response, verification_annotated) =
        annotate_unresolved_verification(&visible_response, turn_evidence);
    let response_usage = visible_response.token_usage();
    let response_cost_cents = crate::services::llm_gateway::compute_cost_cents(
        visible_response.model.as_str(),
        response_usage,
    );
    let response_sequence_num =
        append_brain_response_from_completion(ctx, session_id, &visible_response).await?;
    record_last_response_sequence(ctx, response_sequence_num);
    ingest_deferred_session_turn(
        ctx,
        session_id,
        &request,
        &visible_response,
        response_sequence_num,
    )
    .await?;
    let response_event = brain_response_event_by_sequence(
        ctx,
        session_id,
        turn_context.identity,
        response_sequence_num,
    )
    .await?;
    emit_generation_lineage(
        workflow.lineage.as_ref(),
        turn_id,
        &meta,
        "llm_gateway",
        &request_model,
        &visible_response,
        &citation_sources,
        response_cost_cents,
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
    let selected_procedure_skills = selected_procedure_skill_refs(&request.metadata);
    match dispatch_response_tool_calls(
        workflow,
        ctx,
        RootToolContext {
            meta: &meta,
            session_id,
            active_canary: active_canary.as_deref(),
            trusted_sandbox_manifest: trusted_sandbox_manifest.as_ref(),
            selected_procedure_skills: &selected_procedure_skills,
            turn_evidence,
        },
        &allowed_tools,
        tool_budget,
        &tool_calls,
        last_summary,
    )
    .await?
    {
        ToolDispatchOutcome::Completed => {}
        ToolDispatchOutcome::Cancelled => {
            return Ok(TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled));
        }
        ToolDispatchOutcome::ToolBudgetExceeded(exhaustion) => {
            return Ok(TurnIterationOutcome::ToolBudgetExceeded(exhaustion));
        }
    }

    Ok(TurnIterationOutcome::Core(turn_outcome_for_response(
        &visible_response,
    )))
}

/// Whether per-turn `TurnMetrics` telemetry events should be persisted to the durable log.
///
/// Off by default (zero production log growth); enabled in eval/test via `MOA_PERSIST_TURN_METRICS`.
/// Cached once — reading a process-stable env var is deterministic across Restate replay.
fn persist_turn_metrics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MOA_PERSIST_TURN_METRICS")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
    })
}

/// Appends a per-turn `TurnMetrics` telemetry event when persistence is enabled (else a no-op).
///
/// Snapshots must be taken before calling so the event does not count its own append.
#[allow(clippy::too_many_arguments)]
async fn maybe_append_turn_metrics(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    actor: &str,
    coordination: &moa_core::coordination_counters::CoordinationSnapshot,
    replay: &moa_core::session_replay::TurnReplaySnapshot,
    llm_ms: u64,
    tool_ms: u64,
    persist_ms: u64,
) -> Result<(), HandlerError> {
    if !persist_turn_metrics_enabled() {
        return Ok(());
    }
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::TurnMetrics {
                turn_id: turn_id.to_string(),
                actor: actor.to_string(),
                session_vo_calls: coordination.session_vo_calls,
                worker_vo_calls: coordination.worker_vo_calls,
                vo_sends: coordination.vo_sends,
                durable_appends: coordination.durable_appends,
                get_events_calls: replay.get_events_calls,
                events_bytes: replay.events_bytes,
                llm_ms,
                tool_ms,
                persist_ms,
            },
            dedupe_key: Some(format!("turn_metrics:{turn_id}")),
        }))
        .call()
        .await?;
    Ok(())
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

async fn record_selected_segment_skills(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    for skill_name in selected_skill_names(metadata) {
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_skill_activation(Json(RecordSegmentSkillActivationRequest {
                session_id,
                skill_name,
            }))
            .send();
    }
    Ok(())
}

/// Returns whether the turn selected at least one skill carrying a procedure, so
/// the deterministic procedure execution tools should be offered on this turn.
fn turn_has_procedure_capable_skill(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    metadata
        .get(SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .any(|name| !name.trim().is_empty())
}

/// Returns the normalized `skill://<name>` references for the procedure-capable
/// skills selected on this turn, used to gate which skills `run_procedure` may start.
///
/// This reads the same context metadata that decides whether the procedure tools are
/// offered ([`turn_has_procedure_capable_skill`]) and normalizes each name the way
/// [`moa_core::types::procedure_tools::RunProcedureToolInput::procedure_ref`] does, so a `run_procedure` call
/// and the allowlist compare on identical forms. Both the root and worker turn loops
/// use this so the membership gate shares one source of truth.
pub(crate) fn selected_procedure_skill_refs(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::BTreeSet<String> {
    metadata
        .get(SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(moa_core::types::procedure_tools::normalize_procedure_skill_ref)
        .collect()
}

fn selected_skill_names(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Vec<String> {
    let mut names = metadata
        .get(SELECTED_SKILL_NAMES_METADATA_KEY)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

async fn emit_turn_cap_exceeded(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    max_turns: usize,
) -> Result<(), HandlerError> {
    record_session_error("turn_cap");
    append_session_event(
        ctx,
        session_id,
        Event::Error {
            message: format!("model-loop turn cap reached ({max_turns}), stopping"),
            recoverable: true,
        },
    )
    .await
    .map(|_| ())
}

fn create_turn_span(
    meta: Option<&SessionMeta>,
    prompt: Option<&str>,
    turn_number: usize,
    environment: Option<&str>,
) -> tracing::Span {
    let Some(meta) = meta else {
        return tracing::info_span!(
            "session_turn",
            otel.name = %format!("MOA turn {turn_number}"),
            moa.turn.number = turn_number as i64,
        );
    };
    session_turn_span(meta, prompt, turn_number as i64, environment)
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

fn notify_session_of_outcome(
    ctx: &WorkflowContext<'_>,
    session_id: &str,
    identity: &moa_core::traits::Identity,
    outcome: &TurnOutcome,
) {
    moa_core::coordination_counters::record_vo_send();
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use serde_json::json;

    use super::*;

    #[test]
    fn selected_skill_names_ignores_invalid_values_and_deduplicates() {
        // Pins: skill selection metadata from the context pipeline becomes stable segment evidence.
        let mut metadata = HashMap::new();
        metadata.insert(
            SELECTED_SKILL_NAMES_METADATA_KEY.to_string(),
            json!(["rust", "", "incident-triage", "rust", 42, null]),
        );

        assert_eq!(
            selected_skill_names(&metadata),
            vec!["incident-triage".to_string(), "rust".to_string()]
        );
    }

    #[test]
    fn procedure_tools_offered_only_when_a_procedure_skill_is_selected() {
        // Pins: run_procedure/procedure_status are injected only when the turn
        // selected at least one skill that carries a procedure.
        let mut none = HashMap::new();
        none.insert(
            SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY.to_string(),
            json!([]),
        );
        assert!(!turn_has_procedure_capable_skill(&none));
        // Missing key entirely also means no procedure tools.
        assert!(!turn_has_procedure_capable_skill(&HashMap::new()));

        let mut present = HashMap::new();
        present.insert(
            SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY.to_string(),
            json!(["", "damaged-food-order"]),
        );
        assert!(turn_has_procedure_capable_skill(&present));
    }

    #[test]
    fn selected_procedure_skill_refs_normalizes_and_ignores_blanks() {
        // Pins: the run_procedure membership set is built from the selected procedure
        // skill names, normalized to skill:// references, with blank and non-string
        // metadata entries dropped so the allowlist matches procedure_ref() exactly.
        let mut metadata = HashMap::new();
        metadata.insert(
            SELECTED_PROCEDURE_SKILL_NAMES_METADATA_KEY.to_string(),
            json!([
                "damaged-food-order",
                " ",
                "skill://transaction-dispute",
                7,
                null
            ]),
        );

        assert_eq!(
            selected_procedure_skill_refs(&metadata),
            BTreeSet::from([
                "skill://damaged-food-order".to_string(),
                "skill://transaction-dispute".to_string(),
            ])
        );
        // No selected procedure skills yields an empty set, so run_procedure is rejected.
        assert!(selected_procedure_skill_refs(&HashMap::new()).is_empty());
    }
}
