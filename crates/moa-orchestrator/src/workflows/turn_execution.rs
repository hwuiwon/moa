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

use chrono::Utc;
use moa_brain::lineage::emit_generation_lineage;
use moa_brain::pipeline::delegation_planning::{
    DELEGATION_PLAN_METADATA_KEY, DelegationPlan, DelegationPlanNode, plan_delegation_for_request,
};
use moa_brain::pipeline::segments::{SegmentCompleted, SegmentTracker};
use moa_brain::pipeline::skills::SELECTED_SKILL_NAMES_METADATA_KEY;
use moa_brain::segment_assessment::AssessmentOverride;
use moa_brain::turn_learning::build_segment_learning_bundle;
use moa_brain::turn_segments::{
    SegmentBoundarySequences, assess_segment_events, latest_user_message,
    segment_assessment_to_seq, segment_boundary_sequences, segment_events_for_assessment,
    task_segment_from_active, task_segment_from_completed,
};
use moa_core::wire::session_store::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest, GetEventsRequest,
    GetSegmentBaselineRequest, RecordSegmentSkillActivationRequest, RecordSegmentToolUseRequest,
    RecordSegmentTurnUsageRequest, UpdateSegmentAssessmentRequest,
};
use moa_core::wire::turn::{
    RunTurnRequest, TurnComplexityClass, TurnOutcome, TurnOutcomeKind, TurnPhase, TurnProgress,
    TurnTrigger,
};
use moa_core::{
    ActiveSegment, AgentContext, AssessmentPhase, AttachWorkerResultWaiterInput, CompletionRequest,
    CompletionResponse, DEFER_BRAIN_RESPONSE_METADATA_KEY, DelegationTool, DelegationToolKind,
    Event, EventRange, EventRecord, EventType, GuardrailDecision, GuardrailDirection,
    MarkWorkerChildTerminalInput, MoaError, ModelTier, QueryRewriteResult,
    RemoveWorkerResultWaiterInput, SandboxFile, SegmentId, SessionId, SessionMeta,
    SpawnWorkerInput, SpawnWorkerOutput, TaskSegment, ToolCallContent, ToolCallId, ToolInvocation,
    ToolOutput, TrustedSandboxFileEntry, TrustedSandboxFileManifestPayload,
    TrustedSandboxFileManifestRef, TurnOutcome as CoreTurnOutcome, TurnReplayCounters,
    WorkerChildRef, WorkerTerminalResult, default_worker_budget_tokens, is_child_report_tool_name,
    is_delegation_tool_name, scope_turn_replay_counters,
};
use moa_lineage_citation::ChunkRef;
use moa_lineage_core::TurnId;
use moa_memory_ingest::{IngestionVOClient, ingestion_object_key};
use moa_observability::restate_observability::{
    annotate_restate_handler_span, emit_turn_latency_summary, emit_turn_replay_summary,
    event_persist_span, llm_call_span, session_turn_span, tool_dispatch_span,
};
use moa_observability::{
    TurnLatencyCounters, record_session_error, record_turn_event_persist_duration,
    record_turn_latency, record_turn_llm_call_duration, record_turn_tool_dispatch_duration,
    record_turn_workflow_outcome, scope_turn_latency_counters,
};
use restate_sdk::prelude::*;
use sha2::{Digest, Sha256};
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::brain_bridge::{PreparedTurnRequest, QueryRewriteCacheEntry, prepare_turn_request};
use crate::objects::session::{RegisterAutoDelegationRunInput, SessionClient};
use crate::objects::worker::WorkerClient;
use crate::restate_identity::with_identity_headers;
use crate::services::{llm_gateway::LLMGatewayClient, session_store::RestateSessionStoreClient};
use crate::tool_invocation::governed::{
    GovernedInvocationOrigin, GovernedInvocationOutcome, GovernedInvocationProgress,
    GovernedInvocationRequest, invoke_governed_tool,
    record_segment_tool_use as record_governed_segment_tool_use,
};
use crate::turn::util::{
    TurnEvidence, allowed_tool_names, annotate_unresolved_verification,
    ensure_delegation_tool_schemas, response_tool_calls, stable_tool_call_id,
    summarize_response_text, turn_outcome_for_response,
};
use crate::turn_driver::{
    guardrails as driver_guardrails, learning as driver_learning, model_loop as driver_model_loop,
    progress as driver_progress, segments as driver_segments,
};
use crate::worker_dispatch::MAX_WORKER_FAN_OUT;
use crate::workflows::durable_utc_now;
use crate::workflows::errors::moa_error_to_handler_error;
#[cfg(feature = "skill-learning")]
use crate::workflows::skill_learning::{RunSkillLearningRequest, SkillLearningClient};
use crate::workflows::turn_progress::{
    self, SUMMARY_CALLING_MODEL, SUMMARY_CHECKING_RESULTS, SUMMARY_WORKING,
};
use crate::workflows::turn_responsiveness::{
    ToolBudgetDecision, ToolBudgetExhausted, ToolBudgetState,
    has_recent_target as recent_events_have_target,
};

#[derive(Clone, Debug)]
struct BodyOutcome {
    kind: TurnOutcomeKind,
    message: String,
}

#[derive(Clone, Debug)]
struct BuiltTurnRequest {
    request: CompletionRequest,
    active_canary: Option<String>,
    trusted_sandbox_files: Vec<SandboxFile>,
    trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    citation_sources: Vec<ChunkRef>,
}

#[derive(Clone, Copy)]
struct RunOnceContext<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    identity: &'a moa_core::traits::Identity,
}

const AUTO_DELEGATION_TOOL_INDEX_BASE: usize = 10_000;
const AUTO_DELEGATION_WORKER_MAX_TURNS: u32 = 3;
const AUTO_DELEGATION_ROOT_BASE_TURNS: u32 = 4;
const AUTO_DELEGATION_ROOT_TURNS_PER_READY_NODE: u32 = 2;

enum AutoDelegationOutcome {
    Skipped,
    Scheduled,
    Cancelled,
    ToolBudgetExceeded(ToolBudgetExhausted),
}

enum AutoDelegationFanInOutcome {
    Skipped,
    Continue,
    Cancelled,
}

#[derive(Clone, Debug)]
enum TurnIterationOutcome {
    Core(CoreTurnOutcome),
    ToolBudgetExceeded(ToolBudgetExhausted),
}

#[derive(Clone, Debug)]
enum ToolDispatchOutcome {
    Completed,
    Cancelled,
    ToolBudgetExceeded(ToolBudgetExhausted),
}

enum SegmentAssessmentTarget<'a> {
    Completed(&'a SegmentCompleted),
    Active(&'a ActiveSegment),
}

struct SegmentAssessmentInput<'a> {
    target: SegmentAssessmentTarget<'a>,
    events: &'a [EventRecord],
    next_user_message: Option<&'a str>,
    rewrite: Option<&'a QueryRewriteResult>,
    phase: AssessmentPhase,
    overrides: &'a [AssessmentOverride],
    duration_ms: u64,
    resolution_config: &'a moa_core::ResolutionConfig,
}

impl SegmentAssessmentTarget<'_> {
    fn segment_id(&self) -> SegmentId {
        match self {
            Self::Completed(segment) => segment.segment_id,
            Self::Active(segment) => segment.id,
        }
    }

    fn turn_count(&self) -> u32 {
        match self {
            Self::Completed(segment) => segment.turn_count,
            Self::Active(segment) => segment.turn_count,
        }
    }

    fn token_cost(&self) -> u64 {
        match self {
            Self::Completed(segment) => segment.token_cost,
            Self::Active(segment) => segment.token_cost,
        }
    }

    fn task_segment(
        &self,
        meta: &SessionMeta,
        assessment: &moa_core::SegmentAssessment,
        events: &[EventRecord],
    ) -> TaskSegment {
        match self {
            Self::Completed(segment) => {
                task_segment_from_completed(meta, segment, events, assessment)
            }
            Self::Active(segment) => task_segment_from_active(meta, segment, assessment, None),
        }
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

        driver_progress::set_phase(&ctx, TurnPhase::Compiling);
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
                turn_progress::finish_with_live_delivery(&ctx, session_id, phase.clone()).await?;
                driver_progress::set_phase(&ctx, phase);
                TurnOutcome {
                    turn_id: request.turn_id.clone(),
                    kind: body.kind,
                    message: body.message,
                }
            }
            Err(err) => {
                turn_progress::finish_with_live_delivery(&ctx, session_id, TurnPhase::Failed)
                    .await?;
                driver_progress::set_phase(&ctx, TurnPhase::Failed);
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
        driver_progress::request_cancel(&ctx, reason.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn progress(
        &self,
        ctx: SharedWorkflowContext<'_>,
    ) -> Result<Json<TurnProgress>, HandlerError> {
        annotate_restate_handler_span("TurnExecution", "progress");
        driver_progress::snapshot(&ctx).await
    }
}

async fn execute_turn_inside_workflow(
    ctx: &WorkflowContext<'_>,
    request: &RunTurnRequest,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<BodyOutcome, HandlerError> {
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        return Ok(BodyOutcome {
            kind: TurnOutcomeKind::Cancelled,
            message: reason,
        });
    }

    turn_progress::initialize(ctx).await?;
    turn_progress::enable_live_delivery(ctx);

    let meta = load_session_meta(ctx, session_id).await?;
    let user_sequence_num = match request.trigger {
        TurnTrigger::UserMessage => {
            if let Some(outcome) = evaluate_input_guardrail(
                ctx,
                session_id,
                &request.turn_id,
                &meta,
                &request.user_message,
            )
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

    let recent_target_events = load_recent_target_events(ctx, session_id).await?;
    let has_recent_target = recent_events_have_target(&recent_target_events, user_sequence_num);
    let session_limits = &OrchestratorCtx::current_config().session_limits;
    let request_max_turns =
        root_request_turn_cap_for_auto_delegation(&request.user_message, request.max_turns);
    let loop_plan = driver_model_loop::root_loop_plan(
        driver_model_loop::RootLoopPlanRequest {
            user_text: &request.user_message,
            attachment_count: request.attachments.len(),
            request_max_turns,
            has_recent_target,
            available_tool_count: OrchestratorCtx::current_tool_schemas().len(),
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
            });
        }

        let span_meta = load_session_meta(ctx, session_id).await.ok();
        let turn_root_span = create_turn_span(
            span_meta.as_ref(),
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
            emit_turn_latency_summary(&turn_root_span, turn_number as i64, &turn_latency_snapshot);
            turn_result
        })
        .await?;
        let turn_snapshot = turn_counters.snapshot();
        emit_turn_replay_summary(&turn_root_span, turn_number as i64, &turn_snapshot);

        match turn_outcome {
            TurnIterationOutcome::Core(CoreTurnOutcome::Continue) => continue,
            TurnIterationOutcome::Core(CoreTurnOutcome::Cancelled) => {
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
            TurnIterationOutcome::Core(CoreTurnOutcome::Idle) => {
                assess_current_active_segment(ctx, session_id, AssessmentPhase::Final, &[]).await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message: last_summary.unwrap_or_else(|| "idle".to_string()),
                });
            }
            TurnIterationOutcome::ToolBudgetExceeded(exhaustion) => {
                assess_current_active_segment(
                    ctx,
                    session_id,
                    AssessmentPhase::Final,
                    &[AssessmentOverride::TurnCapExceeded],
                )
                .await?;
                emit_tool_budget_exceeded(ctx, session_id, &exhaustion).await?;
                let message = append_zero_cost_assistant_response(
                    ctx,
                    session_id,
                    &meta,
                    exhaustion.assistant_message(),
                )
                .await?;
                return Ok(BodyOutcome {
                    kind: TurnOutcomeKind::Completed,
                    message,
                });
            }
        }
    }

    assess_current_active_segment(
        ctx,
        session_id,
        AssessmentPhase::Final,
        &[AssessmentOverride::TurnCapExceeded],
    )
    .await?;
    emit_turn_cap_exceeded(ctx, session_id, max_turns).await?;
    let message = append_zero_cost_assistant_response(
        ctx,
        session_id,
        &meta,
        format!(
            "MOA stopped because this turn reached the model-loop turn cap ({max_turns}). Narrow the scope or ask MOA to continue."
        ),
    )
    .await?;
    Ok(BodyOutcome {
        kind: TurnOutcomeKind::Completed,
        message,
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

async fn append_zero_cost_assistant_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<String, HandlerError> {
    append_session_event(
        ctx,
        session_id,
        Event::BrainResponse {
            text: text.clone(),
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
    .await?;
    Ok(text)
}

async fn evaluate_input_guardrail(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    meta: &SessionMeta,
    user_message: &str,
) -> Result<Option<BodyOutcome>, HandlerError> {
    let Some(agent_context) = meta.agent_context.as_ref() else {
        return Ok(None);
    };
    let policy =
        AgentContext::parsed_policy_snapshot(agent_context).map_err(moa_error_to_handler_error)?;
    let Some(stage) = policy.guardrail_policy.stage(GuardrailDirection::Input) else {
        return Ok(None);
    };
    if !stage.is_active() {
        return Ok(None);
    }

    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_id,
        TurnPhase::Compiling,
        SUMMARY_CALLING_MODEL,
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;
    let guardrail_request = crate::guardrails::guardrail_completion_request(
        &OrchestratorCtx::current_config(),
        GuardrailDirection::Input,
        stage,
        user_message,
    );
    let response = ctx
        .service_client::<LLMGatewayClient>()
        .complete(Json::from(guardrail_request))
        .call()
        .await?
        .into_inner();
    let evaluation = crate::guardrails::evaluate_guardrail_response(
        &agent_context.policy_hash,
        GuardrailDirection::Input,
        stage,
        &response,
    );
    append_session_event(ctx, session_id, evaluation.to_event()).await?;

    if matches!(evaluation.decision, GuardrailDecision::Block) {
        let text = driver_guardrails::block_message(driver_guardrails::GuardrailBlockMessage {
            stage,
            fallback: "I can't help with that request.",
        });
        append_session_event(
            ctx,
            session_id,
            Event::BrainResponse {
                text,
                thought_signature: None,
                model: evaluation.model.clone(),
                model_tier: ModelTier::Auxiliary,
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
            },
        )
        .await?;
        return Ok(Some(BodyOutcome {
            kind: TurnOutcomeKind::Completed,
            message: "input guardrail blocked".to_string(),
        }));
    }

    Ok(None)
}

async fn visible_response_after_output_guardrail(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    response: &CompletionResponse,
    turn_id: &str,
) -> Result<(CompletionResponse, bool), HandlerError> {
    if response.text.is_empty() {
        return Ok((response.clone(), false));
    }
    let Some(agent_context) = meta.agent_context.as_ref() else {
        return Ok((response.clone(), false));
    };
    let policy =
        AgentContext::parsed_policy_snapshot(agent_context).map_err(moa_error_to_handler_error)?;
    let Some(stage) = policy.guardrail_policy.stage(GuardrailDirection::Output) else {
        return Ok((response.clone(), false));
    };
    if !stage.is_active() {
        return Ok((response.clone(), false));
    }

    driver_progress::set_phase(ctx, TurnPhase::Persisting);
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_id,
        TurnPhase::Persisting,
        SUMMARY_CHECKING_RESULTS,
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;
    let guardrail_request = crate::guardrails::guardrail_completion_request(
        &OrchestratorCtx::current_config(),
        GuardrailDirection::Output,
        stage,
        &response.text,
    );
    let judge_response = ctx
        .service_client::<LLMGatewayClient>()
        .complete(Json::from(guardrail_request))
        .call()
        .await?
        .into_inner();
    let evaluation = crate::guardrails::evaluate_guardrail_response(
        &agent_context.policy_hash,
        GuardrailDirection::Output,
        stage,
        &judge_response,
    );
    append_session_event(ctx, session_id, evaluation.to_event()).await?;

    if matches!(evaluation.decision, GuardrailDecision::Block) {
        let visible_response =
            driver_guardrails::blocked_output_response(driver_guardrails::BlockedOutputResponse {
                response,
                stage,
            });
        return Ok((visible_response, true));
    }

    Ok((response.clone(), false))
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
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        &progress_turn_id,
        TurnPhase::Compiling,
        SUMMARY_WORKING,
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;
    let Some(built_request) = build_request_inside_workflow(ctx, session_id, turn_id).await? else {
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

    let meta = load_session_meta(ctx, session_id).await?;
    OrchestratorCtx::current_tool_router()
        .set_trusted_sandbox_files(&meta, None, trusted_sandbox_files.clone())
        .await;
    let active_segment = ensure_current_segment(ctx, session_id, &meta, &mut request).await?;
    if let Some(segment) = active_segment.as_ref() {
        driver_segments::insert_active_segment_metadata(&mut request, segment);
        record_selected_segment_skills(ctx, session_id, &request.metadata).await?;
    }
    ensure_delegation_tool_schemas(&mut request);
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
        ctx,
        AutoDelegationContext {
            turn_id: &progress_turn_id,
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

    match maybe_fan_in_auto_delegation_results(ctx, session_id, &progress_turn_id, last_summary)
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
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        &progress_turn_id,
        TurnPhase::Streaming,
        SUMMARY_CALLING_MODEL,
        cadence.first_delay_ms,
        cadence.interval_ms,
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
    let (visible_response, output_blocked) = visible_response_after_output_guardrail(
        ctx,
        session_id,
        &meta,
        &response,
        &progress_turn_id,
    )
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
    ingest_deferred_session_turn(
        ctx,
        session_id,
        &request,
        &visible_response,
        response_sequence_num,
    )
    .await?;
    let response_event = latest_matching_brain_response_event(
        ctx,
        session_id,
        turn_context.identity,
        &visible_response,
    )
    .await?;
    let lineage = OrchestratorCtx::current_lineage();
    emit_generation_lineage(
        lineage.as_ref(),
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
    match dispatch_response_tool_calls(
        ctx,
        RootToolContext {
            turn_id: &progress_turn_id,
            meta: &meta,
            session_id,
            active_canary: active_canary.as_deref(),
            trusted_sandbox_manifest: trusted_sandbox_manifest.as_ref(),
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

async fn build_request_inside_workflow(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: TurnId,
) -> Result<Option<BuiltTurnRequest>, HandlerError> {
    let active_user_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner);
    let cached_query_rewrite = ctx
        .get::<Json<QueryRewriteCacheEntry>>(driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE)
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
            .map_err(moa_error_to_handler_error)
        })
        .name("prepare_turn_request")
        .await?
        .into_inner();
    if let Some(cache) = prepared.query_rewrite_cache {
        ctx.set(
            driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE,
            Json::from(cache),
        );
    } else {
        ctx.clear(driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE);
    }

    Ok(match prepared.prepared {
        PreparedTurnRequest::Idle => None,
        PreparedTurnRequest::Request(request) => {
            let trusted_sandbox_manifest =
                store_trusted_sandbox_manifest(ctx, session_id, &prepared.trusted_sandbox_files)
                    .await?;
            Some(BuiltTurnRequest {
                request: *request,
                active_canary: prepared.active_canary,
                trusted_sandbox_files: prepared.trusted_sandbox_files,
                trusted_sandbox_manifest,
                citation_sources: prepared.citation_sources,
            })
        }
    })
}

async fn store_trusted_sandbox_manifest(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    files: &[SandboxFile],
) -> Result<Option<TrustedSandboxFileManifestRef>, HandlerError> {
    if files.is_empty() {
        return Ok(None);
    }

    let payload = TrustedSandboxFileManifestPayload {
        files: files.to_vec(),
    };
    let payload_text = serde_json::to_string(&payload)
        .map_err(MoaError::from)
        .map_err(moa_error_to_handler_error)?;
    let manifest_sha256 = sha256_hex(payload_text.as_bytes());
    let entries = trusted_sandbox_file_entries(files);
    let store = OrchestratorCtx::current_session_store();
    let claim_check = ctx
        .run(|| async move {
            store
                .store_text_artifact(session_id, &payload_text)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("store_trusted_sandbox_file_manifest")
        .await?
        .into_inner();

    Ok(Some(TrustedSandboxFileManifestRef {
        blob_id: claim_check.blob_id,
        size: claim_check.size,
        manifest_sha256,
        files: entries,
    }))
}

fn trusted_sandbox_file_entries(files: &[SandboxFile]) -> Vec<TrustedSandboxFileEntry> {
    files
        .iter()
        .map(|file| TrustedSandboxFileEntry {
            path: file.path.clone(),
            content_sha256: sha256_hex(&file.content),
            size: file.content.len(),
            executable: file.executable,
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct AutoDelegationContext<'a> {
    turn_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    turn_evidence: &'a mut TurnEvidence,
}

async fn maybe_schedule_auto_delegation(
    ctx: &WorkflowContext<'_>,
    mut schedule_context: AutoDelegationContext<'_>,
    request: &CompletionRequest,
    allowed_tools: &std::collections::BTreeSet<String>,
    tool_budget: &mut ToolBudgetState,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationOutcome, HandlerError> {
    if !allowed_tools.contains(DelegationToolKind::Spawn.name()) {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    let Some(user_sequence_num) = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > 0)
    else {
        return Ok(AutoDelegationOutcome::Skipped);
    };
    let scheduled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if scheduled_sequence_num == Some(user_sequence_num) {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    let Some(plan) = delegation_plan_from_metadata(&request.metadata) else {
        return Ok(AutoDelegationOutcome::Skipped);
    };
    let ready_nodes = ready_delegation_nodes(&plan);
    let worker_slots = available_auto_worker_slots(ctx, schedule_context.session_id).await?;
    let spawn_count = ready_nodes
        .len()
        .min(MAX_WORKER_FAN_OUT)
        .min(worker_slots)
        .min(tool_budget.remaining_tool_calls());
    if spawn_count == 0 {
        return Ok(AutoDelegationOutcome::Skipped);
    }

    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let tool_subset = auto_worker_tool_subset(allowed_tools);
    let mut worker_ids = Vec::new();
    for (index, node) in ready_nodes.into_iter().take(spawn_count).enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(AutoDelegationOutcome::Cancelled);
        }

        let spawn_input = auto_spawn_input(&plan, node, &tool_subset);
        let tool_call = auto_spawn_tool_call(
            user_sequence_num,
            AUTO_DELEGATION_TOOL_INDEX_BASE + index,
            node,
            &spawn_input,
        )?;
        if let Some(exhaustion) =
            record_tool_budget(ctx, tool_budget, &tool_call.invocation).await?
        {
            return Ok(AutoDelegationOutcome::ToolBudgetExceeded(exhaustion));
        }

        let worker_id = dispatch_auto_delegation_spawn(
            ctx,
            &mut schedule_context,
            index,
            tool_call,
            spawn_input,
        )
        .await?;
        worker_ids.push(worker_id);
    }

    register_auto_delegation_run(
        ctx,
        schedule_context.session_id,
        user_sequence_num,
        worker_ids.clone(),
    )
    .await?;
    ctx.set(
        driver_progress::RootTurnStateKey::AUTO_DELEGATION_WORKER_IDS,
        Json::from(worker_ids),
    );
    ctx.set(
        driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE,
        Json::from(user_sequence_num),
    );
    Ok(AutoDelegationOutcome::Scheduled)
}

async fn register_auto_delegation_run(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    user_sequence_num: u64,
    worker_ids: Vec<String>,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .register_auto_delegation_run(Json::from(RegisterAutoDelegationRunInput {
            user_sequence_num,
            worker_ids,
        }))
        .call()
        .await?;
    Ok(())
}

async fn maybe_fan_in_auto_delegation_results(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationFanInOutcome, HandlerError> {
    let Some(user_sequence_num) = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .filter(|sequence_num| *sequence_num > 0)
    else {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    };
    let scheduled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if scheduled_sequence_num != Some(user_sequence_num) {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }
    let bundled_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_SEQUENCE)
        .await?
        .map(Json::into_inner);
    if bundled_sequence_num == Some(user_sequence_num) {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }

    let worker_ids = ctx
        .get::<Json<Vec<String>>>(driver_progress::RootTurnStateKey::AUTO_DELEGATION_WORKER_IDS)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    if worker_ids.is_empty() {
        return Ok(AutoDelegationFanInOutcome::Skipped);
    }

    let children = session_child_refs(ctx, session_id).await?;
    match auto_delegation_fan_in_readiness(&worker_ids, &children) {
        AutoDelegationFanInReadiness::Complete(results) => {
            append_auto_delegation_result_bundle(ctx, session_id, user_sequence_num, results)
                .await?;
            ctx.set(
                driver_progress::RootTurnStateKey::AUTO_DELEGATION_FAN_IN_SEQUENCE,
                Json::from(user_sequence_num),
            );
            Ok(AutoDelegationFanInOutcome::Continue)
        }
        AutoDelegationFanInReadiness::Pending(worker_id) => {
            wait_for_auto_delegation_worker(ctx, session_id, turn_id, &worker_id, last_summary)
                .await
        }
        AutoDelegationFanInReadiness::Unavailable => Ok(AutoDelegationFanInOutcome::Skipped),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AutoDelegationFanInReadiness {
    Complete(Vec<WorkerTerminalResult>),
    Pending(String),
    Unavailable,
}

fn auto_delegation_fan_in_readiness(
    worker_ids: &[String],
    children: &[WorkerChildRef],
) -> AutoDelegationFanInReadiness {
    let mut results = Vec::with_capacity(worker_ids.len());
    for worker_id in worker_ids {
        let Some(child) = children.iter().find(|child| child.id == *worker_id) else {
            return AutoDelegationFanInReadiness::Unavailable;
        };
        let Some(terminal) = child.terminal.as_ref() else {
            return AutoDelegationFanInReadiness::Pending(worker_id.clone());
        };
        results.push(terminal.clone());
    }
    AutoDelegationFanInReadiness::Complete(results)
}

async fn wait_for_auto_delegation_worker(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    turn_id: &str,
    worker_id: &str,
    last_summary: &mut Option<String>,
) -> Result<AutoDelegationFanInOutcome, HandlerError> {
    if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
        *last_summary = Some(reason);
        return Ok(AutoDelegationFanInOutcome::Cancelled);
    }

    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        session_id,
        turn_id,
        TurnPhase::Tooling,
        SUMMARY_CHECKING_RESULTS,
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;

    let (awakeable_id, terminal_future) = ctx.awakeable::<String>();
    let attached = ctx
        .object_client::<WorkerClient>(worker_id.to_string())
        .attach_result_waiter(Json::from(AttachWorkerResultWaiterInput {
            awakeable_id: awakeable_id.clone(),
        }))
        .call()
        .await?
        .into_inner();
    if let Some(terminal) = attached.terminal {
        cache_auto_delegation_terminal(ctx, session_id, worker_id, terminal).await?;
        return Ok(AutoDelegationFanInOutcome::Continue);
    }

    restate_sdk::select! {
        reason = ctx.promise::<String>(driver_progress::TurnStateKey::CANCEL_REASON_PROMISE) => {
            remove_auto_delegation_result_waiter(ctx, worker_id, awakeable_id).await?;
            let reason = reason?;
            *last_summary = Some(reason);
            Ok(AutoDelegationFanInOutcome::Cancelled)
        },
        terminal = terminal_future => {
            let terminal = terminal?;
            let terminal = serde_json::from_str::<WorkerTerminalResult>(&terminal).map_err(|error| {
                TerminalError::new(format!(
                    "failed to decode auto delegation terminal result: {error}"
                ))
            })?;
            cache_auto_delegation_terminal(ctx, session_id, worker_id, terminal).await?;
            Ok(AutoDelegationFanInOutcome::Continue)
        },
        _ = ctx.sleep(Duration::from_millis(crate::delegation::MAX_WAIT_TIMEOUT_MS)) => {
            remove_auto_delegation_result_waiter(ctx, worker_id, awakeable_id).await?;
            Ok(AutoDelegationFanInOutcome::Continue)
        }
    }
}

async fn cache_auto_delegation_terminal(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    worker_id: &str,
    terminal: WorkerTerminalResult,
) -> Result<(), HandlerError> {
    ctx.object_client::<SessionClient>(session_id.to_string())
        .mark_child_terminal(Json::from(MarkWorkerChildTerminalInput {
            worker_id: worker_id.to_string(),
            terminal,
        }))
        .call()
        .await?;
    Ok(())
}

async fn remove_auto_delegation_result_waiter(
    ctx: &WorkflowContext<'_>,
    worker_id: &str,
    awakeable_id: String,
) -> Result<(), HandlerError> {
    ctx.object_client::<WorkerClient>(worker_id.to_string())
        .remove_result_waiter(Json::from(RemoveWorkerResultWaiterInput { awakeable_id }))
        .call()
        .await?;
    Ok(())
}

fn delegation_plan_from_metadata(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<DelegationPlan> {
    metadata
        .get(DELEGATION_PLAN_METADATA_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn ready_delegation_nodes(plan: &DelegationPlan) -> Vec<&DelegationPlanNode> {
    plan.nodes
        .iter()
        .filter(|node| node.depends_on.is_empty())
        .collect()
}

fn root_request_turn_cap_for_auto_delegation(
    user_message: &str,
    request_max_turns: Option<u32>,
) -> Option<u32> {
    let Some(delegation_cap) = auto_delegation_root_turn_cap(user_message) else {
        return request_max_turns;
    };
    Some(request_max_turns.map_or(delegation_cap, |cap| cap.max(delegation_cap)))
}

fn auto_delegation_root_turn_cap(user_message: &str) -> Option<u32> {
    let plan = plan_delegation_for_request(user_message)?;
    let ready_node_count = ready_delegation_nodes(&plan).len().min(MAX_WORKER_FAN_OUT);
    if ready_node_count == 0 {
        return None;
    }
    let ready_node_count = u32::try_from(ready_node_count).unwrap_or(u32::MAX);
    Some(
        AUTO_DELEGATION_ROOT_BASE_TURNS.saturating_add(
            ready_node_count.saturating_mul(AUTO_DELEGATION_ROOT_TURNS_PER_READY_NODE),
        ),
    )
}

fn auto_worker_tool_subset(allowed_tools: &std::collections::BTreeSet<String>) -> Vec<String> {
    allowed_tools
        .iter()
        .filter(|name| !is_delegation_tool_name(name) && !is_child_report_tool_name(name))
        .cloned()
        .collect()
}

async fn available_auto_worker_slots(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<usize, HandlerError> {
    let children = session_child_refs(ctx, session_id).await?;
    Ok(remaining_worker_capacity(&children))
}

async fn session_child_refs(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<WorkerChildRef>, HandlerError> {
    Ok(ctx
        .object_client::<SessionClient>(session_id.to_string())
        .child_refs()
        .call()
        .await?
        .into_inner())
}

fn remaining_worker_capacity(children: &[WorkerChildRef]) -> usize {
    let active_children = children
        .iter()
        .filter(|child| child.terminal.is_none())
        .count();
    MAX_WORKER_FAN_OUT.saturating_sub(active_children)
}

fn auto_spawn_input(
    plan: &DelegationPlan,
    node: &DelegationPlanNode,
    tool_subset: &[String],
) -> SpawnWorkerInput {
    SpawnWorkerInput {
        task: format!(
            "Complete this coordinator-delegated subtask.\n\n\
             Delegation reason: {}\n\
             Subtask: {}\n\n\
             Return the outcome, evidence, and any unresolved blocker to the coordinator. \
             Use the available session context and return a best-effort partial result \
             when source material is missing. Request user input only when no useful \
             outcome, evidence, or next-check recommendation can be produced.",
            plan.reason, node.title
        ),
        task_name: Some(node.title.clone()),
        tool_subset: tool_subset.to_vec(),
        budget_tokens: default_worker_budget_tokens(),
        max_turns: Some(AUTO_DELEGATION_WORKER_MAX_TURNS),
    }
}

fn auto_spawn_tool_call(
    user_sequence_num: u64,
    stable_index: usize,
    node: &DelegationPlanNode,
    spawn_input: &SpawnWorkerInput,
) -> Result<ToolCallContent, HandlerError> {
    Ok(ToolCallContent {
        invocation: ToolInvocation {
            id: Some(auto_delegation_provider_tool_id(
                user_sequence_num,
                stable_index,
                &node.id,
            )),
            name: DelegationToolKind::Spawn.name().to_string(),
            input: serde_json::to_value(spawn_input).map_err(|error| {
                TerminalError::new(format!(
                    "failed to serialize auto delegation input: {error}"
                ))
            })?,
        },
        provider_metadata: None,
    })
}

fn auto_delegation_provider_tool_id(
    user_sequence_num: u64,
    stable_index: usize,
    node_id: &str,
) -> String {
    format!(
        "fc_auto_delegation_{user_sequence_num}_{stable_index}_{}",
        provider_safe_id_segment(node_id)
    )
}

fn provider_safe_id_segment(value: &str) -> String {
    let safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let safe = safe.trim_matches('_');
    if safe.is_empty() {
        "node".to_string()
    } else {
        safe.to_string()
    }
}

async fn dispatch_auto_delegation_spawn(
    ctx: &WorkflowContext<'_>,
    schedule_context: &mut AutoDelegationContext<'_>,
    index: usize,
    tool_call: ToolCallContent,
    spawn_input: SpawnWorkerInput,
) -> Result<String, HandlerError> {
    let invocation = tool_call.invocation.clone();
    let tool_id = stable_tool_call_id(
        schedule_context.session_id,
        AUTO_DELEGATION_TOOL_INDEX_BASE + index,
        &tool_call,
    );
    append_tool_call_event(ctx, schedule_context.session_id, tool_id, &tool_call).await?;

    let cadence = driver_progress::current_cadence();
    turn_progress::maybe_emit(
        ctx,
        schedule_context.session_id,
        schedule_context.turn_id,
        TurnPhase::Tooling,
        turn_progress::running_tool_summary(&invocation.name),
        cadence.first_delay_ms,
        cadence.interval_ms,
    )
    .await?;

    let span = tool_dispatch_span(&invocation.name);
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession {
            session_id: schedule_context.session_id,
            meta: schedule_context.meta,
        },
        DelegationTool::Spawn(spawn_input),
        schedule_context.trusted_sandbox_manifest,
    )
    .instrument(span)
    .await?;
    record_turn_tool_dispatch_duration(dispatch_started.elapsed(), 1);
    let worker_id = spawn_worker_id_from_output(&output)?;

    append_auto_delegation_result(
        ctx,
        schedule_context.session_id,
        tool_id,
        &invocation,
        output,
        schedule_context.turn_evidence,
    )
    .await?;
    Ok(worker_id)
}

fn spawn_worker_id_from_output(output: &ToolOutput) -> Result<String, HandlerError> {
    let structured = output
        .structured
        .clone()
        .ok_or_else(|| TerminalError::new("spawn_worker returned no structured output"))?;
    let output = serde_json::from_value::<SpawnWorkerOutput>(structured).map_err(|error| {
        TerminalError::new(format!(
            "failed to decode auto delegation spawn output: {error}"
        ))
    })?;
    Ok(output.worker_id)
}

async fn append_auto_delegation_result(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: ToolOutput,
    turn_evidence: &mut TurnEvidence,
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
    .await?;
    turn_evidence.record_tool_result(invocation, &output);

    if !output.is_error {
        record_segment_tool_use(ctx, session_id, &invocation.name).await?;
    }
    Ok(())
}

async fn append_auto_delegation_result_bundle(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    user_sequence_num: u64,
    results: Vec<WorkerTerminalResult>,
) -> Result<(), HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::WorkerResultBundle {
                user_sequence_num,
                results,
            },
            dedupe_key: Some(format!("auto_delegation_fan_in:{user_sequence_num}")),
        }))
        .call()
        .instrument(persist_span)
        .await?;
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(())
}

struct RootToolContext<'a> {
    turn_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    active_canary: Option<&'a str>,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
    turn_evidence: &'a mut TurnEvidence,
}

async fn dispatch_response_tool_calls(
    ctx: &WorkflowContext<'_>,
    mut tool_context: RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    tool_budget: &mut ToolBudgetState,
    tool_calls: &[&ToolCallContent],
    last_summary: &mut Option<String>,
) -> Result<ToolDispatchOutcome, HandlerError> {
    for (index, tool_call) in tool_calls.iter().enumerate() {
        if let Some(reason) = driver_progress::cancel_requested(ctx).await? {
            *last_summary = Some(reason);
            return Ok(ToolDispatchOutcome::Cancelled);
        }
        if let Some(exhaustion) =
            record_tool_budget(ctx, tool_budget, &tool_call.invocation).await?
        {
            return Ok(ToolDispatchOutcome::ToolBudgetExceeded(exhaustion));
        }
        handle_tool_call(ctx, &mut tool_context, allowed_tools, index, tool_call).await?;
    }
    Ok(ToolDispatchOutcome::Completed)
}

async fn record_tool_budget(
    ctx: &WorkflowContext<'_>,
    tool_budget: &mut ToolBudgetState,
    invocation: &ToolInvocation,
) -> Result<Option<ToolBudgetExhausted>, HandlerError> {
    match tool_budget.before_tool_dispatch(invocation) {
        ToolBudgetDecision::Allow {
            attempted_tool_calls,
        } => {
            driver_progress::set_tool_calls(ctx, attempted_tool_calls);
            Ok(None)
        }
        ToolBudgetDecision::Stop(exhaustion) => {
            driver_progress::set_tool_calls(ctx, tool_budget.attempted_tool_calls());
            Ok(Some(exhaustion))
        }
    }
}

async fn handle_tool_call(
    ctx: &WorkflowContext<'_>,
    tool_context: &mut RootToolContext<'_>,
    allowed_tools: &std::collections::BTreeSet<String>,
    index: usize,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    driver_progress::set_phase(ctx, TurnPhase::Tooling);
    let meta = tool_context.meta;
    let session_id = tool_context.session_id;
    let active_canary = tool_context.active_canary;
    let turn_id = tool_context.turn_id;
    let turn_evidence = &mut *tool_context.turn_evidence;
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
            active_canary,
            trusted_sandbox_manifest: tool_context.trusted_sandbox_manifest,
            origin: GovernedInvocationOrigin::RootTurn,
            progress: GovernedInvocationProgress {
                turn_id,
                first_delay_ms: cadence.first_delay_ms,
                interval_ms: cadence.interval_ms,
            },
        },
    )
    .await?;

    match outcome {
        GovernedInvocationOutcome::Completed(result) => {
            turn_evidence.record_tool_result(&result.invocation, &result.output);
            if result.should_record_segment_tool_use() {
                record_governed_segment_tool_use(ctx, session_id, &result.invocation.name).await?;
            }
        }
        GovernedInvocationOutcome::Delegation { tool_id, .. } => {
            handle_delegation_tool(
                ctx,
                DelegationToolRequest {
                    turn_id,
                    meta,
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

struct DelegationToolRequest<'a> {
    turn_id: &'a str,
    meta: &'a SessionMeta,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &'a ToolCallContent,
    trusted_sandbox_manifest: Option<&'a TrustedSandboxFileManifestRef>,
}

async fn handle_delegation_tool(
    ctx: &WorkflowContext<'_>,
    request: DelegationToolRequest<'_>,
    turn_evidence: &mut TurnEvidence,
) -> Result<(), HandlerError> {
    let DelegationToolRequest {
        turn_id,
        meta,
        session_id,
        tool_id,
        tool_call,
        trusted_sandbox_manifest,
    } = request;
    let invocation = tool_call.invocation.clone();
    append_tool_call_event(ctx, session_id, tool_id, tool_call).await?;
    let Some(tool) = moa_core::DelegationTool::from_invocation(&invocation)
        .map_err(moa_error_to_handler_error)?
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
    let dispatch_started = Instant::now();
    let output = crate::delegation::execute_delegation_tool(
        ctx,
        crate::delegation::DelegationParent::RootSession { session_id, meta },
        tool,
        trusted_sandbox_manifest,
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
    turn_evidence.record_tool_result(&invocation, &output);

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

    let now = durable_utc_now(ctx, "workflow_utc_now").await?;
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
                    dedupe_key: None,
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
            .call()
            .await?;
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: transition.started.clone().into_event(),
                dedupe_key: None,
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
    completed: &SegmentCompleted,
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
    let rewrite = driver_segments::query_rewrite_from_metadata(metadata);
    let phase = if next_user_message.is_some() {
        AssessmentPhase::Deferred
    } else {
        AssessmentPhase::Immediate
    };
    let resolution_config = OrchestratorCtx::current_config().resolution.clone();
    assess_and_persist_segment(
        ctx,
        meta,
        SegmentAssessmentInput {
            target: SegmentAssessmentTarget::Completed(completed),
            events: &segment_events,
            next_user_message: next_user_message.as_deref(),
            rewrite: rewrite.as_ref(),
            phase,
            overrides: &[],
            duration_ms: completed.duration_ms,
            resolution_config: &resolution_config,
        },
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
    let duration_ms = durable_utc_now(ctx, "workflow_utc_now")
        .await?
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    assess_and_persist_segment(
        ctx,
        &meta,
        SegmentAssessmentInput {
            target: SegmentAssessmentTarget::Active(&segment),
            events: &segment_events,
            next_user_message: None,
            rewrite: None,
            phase,
            overrides,
            duration_ms,
            resolution_config: &runtime.config().resolution,
        },
    )
    .await?;
    Ok(())
}

async fn assess_and_persist_segment(
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    input: SegmentAssessmentInput<'_>,
) -> Result<(), HandlerError> {
    let baseline = load_segment_baseline(ctx, meta.tenant_id).await?;
    let assessment = assess_segment_events(
        input.events,
        input.target.turn_count(),
        input.target.token_cost(),
        input.duration_ms,
        baseline.as_ref(),
        input.next_user_message,
        input.rewrite.is_some_and(|rewrite| rewrite.is_new_task),
        input.phase,
        input.overrides,
        input.resolution_config,
    );
    let segment_id = input.target.segment_id();
    record_segment_assessment_learning(ctx, meta.tenant_id, segment_id, &assessment).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_assessment(Json(UpdateSegmentAssessmentRequest {
            segment_id,
            assessment: assessment.clone(),
        }))
        .call()
        .await?;
    let task_segment = input.target.task_segment(meta, &assessment, input.events);
    emit_experience_for_assessment(
        ctx,
        meta,
        &task_segment,
        &assessment,
        input.events,
        input.rewrite,
        Some(input.duration_ms),
    )
    .await
}

fn tenant_key(meta: &SessionMeta) -> String {
    meta.tenant_id.to_string()
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
    let now = durable_utc_now(ctx, "workflow_utc_now").await?;
    let learning = build_segment_learning_bundle(
        meta,
        segment,
        assessment,
        segment_events,
        rewrite,
        duration_ms,
        now,
    );
    let runtime = OrchestratorCtx::current();
    let store = runtime.session_store();
    #[cfg(feature = "skill-learning")]
    let experience_id = learning.experience.id;
    #[cfg(feature = "skill-learning")]
    let min_skill_learning_tool_calls = runtime.config().learning.skills.min_tool_calls;
    let learning_error = ctx
        .run(move || {
            let store = store.clone();
            let learning = learning.clone();
            async move {
                let result = async {
                    store.append_experience_record(&learning.experience).await?;
                    store
                        .append_experience_attributions(&learning.attributions)
                        .await?;
                    for candidate in &learning.candidates {
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
    if driver_learning::skill_learning_dispatch_is_eligible(
        segment_events,
        min_skill_learning_tool_calls,
    ) {
        dispatch_skill_learning_after_experience(ctx, meta.id, experience_id).await?;
    }
    Ok(())
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
        let entry = driver_learning::segment_assessment_learning_entry(
            driver_learning::SegmentAssessmentLearningRequest {
                id: uuid::Uuid::now_v7(),
                tenant_id,
                segment_id,
                assessment: &assessment,
                valid_from: Utc::now(),
            },
        )
        .map_err(HandlerError::from)?;
        session_store
            .append_learning(&entry)
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

async fn load_recent_target_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    load_events_in_range(
        ctx,
        session_id,
        EventRange {
            event_types: Some(vec![
                EventType::SegmentStarted,
                EventType::SegmentCompleted,
                EventType::UserMessage,
                EventType::BrainResponse,
                EventType::ToolCall,
                EventType::ToolResult,
                EventType::ToolError,
                EventType::WorkerSpawned,
                EventType::WorkerMessageSent,
                EventType::MemoryRead,
                EventType::MemoryWrite,
                EventType::MemoryIngest,
            ]),
            ..EventRange::recent(24)
        },
        "turn_execution_load_recent_target_events",
    )
    .await
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
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
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

async fn emit_tool_budget_exceeded(
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

fn turn_outcome_kind_label(kind: &TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
    }
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
    fn auto_delegation_turn_cap_raises_low_explicit_root_cap() {
        // Pins: multi-worker fan-out leaves enough coordinator turns for wait and synthesis.
        let request =
            "Plan an A/B test readout using activation, retention, and support-ticket signals.";

        assert_eq!(
            root_request_turn_cap_for_auto_delegation(request, Some(6)),
            Some(10)
        );
    }

    #[test]
    fn auto_delegation_turn_cap_keeps_higher_explicit_root_cap() {
        // Pins: caller-provided headroom is not reduced by deterministic delegation planning.
        let request =
            "Plan an A/B test readout using activation, retention, and support-ticket signals.";

        assert_eq!(
            root_request_turn_cap_for_auto_delegation(request, Some(14)),
            Some(14)
        );
    }

    #[test]
    fn auto_delegation_turn_cap_leaves_non_delegable_turns_unchanged() {
        // Pins: direct asks keep their original responsiveness cap.
        assert_eq!(
            root_request_turn_cap_for_auto_delegation("What is the status?", Some(6)),
            Some(6)
        );
        assert_eq!(
            root_request_turn_cap_for_auto_delegation("What is the status?", None),
            None
        );
    }

    #[test]
    fn auto_delegation_uses_only_ready_dag_nodes() {
        // Pins: deterministic scheduling can parallelize ready work without crossing dependencies.
        let plan = DelegationPlan {
            reason: "explicit_multi_workstream_list".to_string(),
            nodes: vec![
                DelegationPlanNode {
                    id: "node-1".to_string(),
                    title: "support tickets".to_string(),
                    depends_on: Vec::new(),
                },
                DelegationPlanNode {
                    id: "node-2".to_string(),
                    title: "billing logs".to_string(),
                    depends_on: Vec::new(),
                },
                DelegationPlanNode {
                    id: "node-3".to_string(),
                    title: "final synthesis".to_string(),
                    depends_on: vec!["node-1".to_string(), "node-2".to_string()],
                },
            ],
        };

        let ready = ready_delegation_nodes(&plan)
            .into_iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ready, vec!["node-1", "node-2"]);
    }

    #[test]
    fn auto_delegation_worker_subset_filters_control_tools() {
        // Pins: auto-spawned workers inherit execution tools, not coordinator or child-control tools.
        let allowed_tools = BTreeSet::from([
            "cancel_worker".to_string(),
            "file_read".to_string(),
            "report_to_parent".to_string(),
            "spawn_worker".to_string(),
            "web_fetch".to_string(),
        ]);

        assert_eq!(
            auto_worker_tool_subset(&allowed_tools),
            vec!["file_read".to_string(), "web_fetch".to_string()]
        );
    }

    #[test]
    fn auto_delegation_capacity_ignores_terminal_children() {
        // Pins: deterministic scheduling does not fail a turn when active worker slots are full.
        let active = WorkerChildRef {
            id: "active-worker".to_string(),
            task_hash: "active".to_string(),
            budget_tokens: 128,
            terminal: None,
        };
        let terminal = WorkerChildRef {
            id: "done-worker".to_string(),
            task_hash: "done".to_string(),
            budget_tokens: 128,
            terminal: Some(moa_core::WorkerTerminalResult {
                state: moa_core::WorkerState::Completed,
                result: moa_core::WorkerResult {
                    worker_id: "done-worker".to_string(),
                    success: true,
                    output: "done".to_string(),
                    tokens_used: 32,
                    tools_invoked: 0,
                    error: None,
                },
            }),
        };

        let mut children = vec![active; MAX_WORKER_FAN_OUT];
        assert_eq!(remaining_worker_capacity(&children), 0);

        children.pop();
        children.push(terminal);
        assert_eq!(remaining_worker_capacity(&children), 1);
    }

    #[test]
    fn auto_delegation_fan_in_collects_terminal_results_in_scheduled_order() {
        // Pins: the coordinator synthesis bundle follows DAG scheduling order, not VO child order.
        let worker_ids = vec!["worker-b".to_string(), "worker-a".to_string()];
        let children = vec![
            worker_child("worker-a", Some(worker_terminal("worker-a", "activation"))),
            worker_child("worker-b", Some(worker_terminal("worker-b", "retention"))),
        ];

        let readiness = auto_delegation_fan_in_readiness(&worker_ids, &children);

        let AutoDelegationFanInReadiness::Complete(results) = readiness else {
            panic!("expected complete fan-in readiness");
        };
        assert_eq!(
            results
                .iter()
                .map(|terminal| terminal.result.worker_id.as_str())
                .collect::<Vec<_>>(),
            vec!["worker-b", "worker-a"]
        );
    }

    #[test]
    fn auto_delegation_fan_in_waits_for_first_pending_worker() {
        // Pins: deterministic fan-in blocks before the model call until tracked workers finish.
        let worker_ids = vec!["worker-a".to_string(), "worker-b".to_string()];
        let children = vec![
            worker_child("worker-a", Some(worker_terminal("worker-a", "activation"))),
            worker_child("worker-b", None),
        ];

        assert_eq!(
            auto_delegation_fan_in_readiness(&worker_ids, &children),
            AutoDelegationFanInReadiness::Pending("worker-b".to_string())
        );
    }

    #[test]
    fn auto_delegation_fan_in_skips_when_tracked_worker_is_unavailable() {
        // Pins: a missing child ref does not spin the coordinator loop forever.
        let worker_ids = vec!["worker-a".to_string(), "missing-worker".to_string()];
        let children = vec![worker_child(
            "worker-a",
            Some(worker_terminal("worker-a", "activation")),
        )];

        assert_eq!(
            auto_delegation_fan_in_readiness(&worker_ids, &children),
            AutoDelegationFanInReadiness::Unavailable
        );
    }

    #[test]
    fn auto_delegation_spawn_input_is_generic_and_bounded() {
        // Pins: scheduling keeps `spawn_worker.task` as the generic envelope and applies child caps.
        let plan = DelegationPlan {
            reason: "explicit_comparison".to_string(),
            nodes: Vec::new(),
        };
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "finance assumptions".to_string(),
            depends_on: Vec::new(),
        };

        let input = auto_spawn_input(&plan, &node, &["file_read".to_string()]);

        assert_eq!(input.task_name.as_deref(), Some("finance assumptions"));
        assert_eq!(input.tool_subset, vec!["file_read".to_string()]);
        assert_eq!(input.budget_tokens, default_worker_budget_tokens());
        assert_eq!(input.max_turns, Some(AUTO_DELEGATION_WORKER_MAX_TURNS));
        assert!(input.task.contains("Subtask: finance assumptions"));
        assert!(input.task.contains("best-effort partial result"));
        assert!(input.task.contains("Request user input only"));
    }

    #[test]
    fn auto_delegation_tool_call_looks_like_spawn_worker() {
        // Pins: deterministic auto-spawns are represented as ordinary spawn_worker tool calls.
        let input = SpawnWorkerInput {
            task: "Review support tickets.".to_string(),
            task_name: Some("support tickets".to_string()),
            tool_subset: vec!["file_read".to_string()],
            budget_tokens: 512,
            max_turns: Some(2),
        };
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "support tickets".to_string(),
            depends_on: Vec::new(),
        };

        let tool_call = auto_spawn_tool_call(42, 10_000, &node, &input)
            .expect("spawn tool call should serialize");

        assert_eq!(tool_call.invocation.name, "spawn_worker");
        assert_eq!(
            tool_call.invocation.id.as_deref(),
            Some("fc_auto_delegation_42_10000_node_1")
        );
        let provider_id = tool_call
            .invocation
            .id
            .as_ref()
            .expect("auto-delegation tool call should have provider id");
        assert!(provider_id.starts_with("fc_"));
        assert!(
            provider_id
                .chars()
                .all(|ch| { ch.is_ascii_alphanumeric() || ch == '_' })
        );
        assert_eq!(
            tool_call.invocation.input["task"],
            json!("Review support tickets.")
        );
    }

    #[test]
    fn auto_delegation_tool_call_sanitizes_node_id_for_provider_replay() {
        // Pins: synthetic tool calls must be replayable through providers that only accept
        // letters, numbers, underscores, or dashes in call ids.
        let input = SpawnWorkerInput {
            task: "Review support tickets.".to_string(),
            task_name: Some("support tickets".to_string()),
            tool_subset: vec!["file_read".to_string()],
            budget_tokens: 512,
            max_turns: Some(2),
        };
        let node = DelegationPlanNode {
            id: "finance:model/v1".to_string(),
            title: "support tickets".to_string(),
            depends_on: Vec::new(),
        };

        let tool_call = auto_spawn_tool_call(42, 10_000, &node, &input)
            .expect("spawn tool call should serialize");

        assert_eq!(
            tool_call.invocation.id.as_deref(),
            Some("fc_auto_delegation_42_10000_finance_model_v1")
        );
    }

    fn worker_child(id: &str, terminal: Option<moa_core::WorkerTerminalResult>) -> WorkerChildRef {
        WorkerChildRef {
            id: id.to_string(),
            task_hash: format!("hash-{id}"),
            budget_tokens: 128,
            terminal,
        }
    }

    fn worker_terminal(worker_id: &str, output: &str) -> moa_core::WorkerTerminalResult {
        moa_core::WorkerTerminalResult {
            state: moa_core::WorkerState::Completed,
            result: moa_core::WorkerResult {
                worker_id: worker_id.to_string(),
                success: true,
                output: output.to_string(),
                tokens_used: 17,
                tools_invoked: 1,
                error: None,
            },
        }
    }
}
