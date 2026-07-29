//! Target execution paths for behavior-lab trial workflows.

use super::finalize::failure_outcome;
use super::status::{attach_trial_execution_run, attach_trial_session, increment_trial_turn};
use super::trial_simulator::{SimulatorContext, simulator_done, simulator_next_user_message};
use super::*;
use crate::objects::session::{AttachSessionTurnWaiterInput, RemoveSessionTurnWaiterInput};
use crate::services::execution::ExecutionClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::durable_utc_now;
use moa_artifacts::{
    canonical::canonical_json_bytes as artifact_canonical_json_bytes,
    execution_plan::{ExecutionGoalContract, GeneratedExecutionCandidate},
    reference::ArtifactRef,
};
use moa_config::MoaConfig;
use moa_core::types::{
    agent::AgentContext,
    contact::{ClientMessageId, ContactId, ContactRef, ContactVerificationState},
    execution_planning::{
        ExecutionAuditViolation, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, ExecutionSourceProvenance,
        PinnedExecutionTemplateRef, bounded_audit_report, canonical_json_bytes,
        execution_planning_hash,
    },
};
use moa_execution::{
    CompileExecutionOutcome, CompileExecutionRequest, ExecutionValidationReport,
    ExecutionValidationSeverity, compile,
    repository::{CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope},
    schema::validate_instance,
    state::ExecutionRunStatus,
    wire::{
        ExecutionCancelRequest, ExecutionPlanningContextRequest, ExecutionRunRequest,
        ExecutionStartRequest, ExecutionStatusResponse,
    },
};
use moa_experiments::evidence::{TrialScoreTarget, TrialTerminalEvidence, TrialTerminalOutcome};
use moa_wire::session_store::AppendEventRequest;
use std::{str::FromStr, time::Instant};

const EXECUTION_TARGET_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const EXECUTION_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const EXPERIMENT_EXECUTION_SESSION_NAMESPACE: Uuid =
    Uuid::from_u128(0xc2a6_731c_2d80_5d4a_9d10_2d20_1283_c6ec);
const EXPERIMENT_EXECUTION_SESSION_DOMAIN: &str = "moa.experiment.execution-session";
const K_TARGET_USAGE_START_SEQUENCE: &str = "target_usage_start_sequence";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetObservation {
    status: SessionStatus,
    latest_response: Option<String>,
    latest_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetUsageObservation {
    latest_sequence: u64,
    tokens: u64,
    cost_cents: u64,
    latest_response: Option<String>,
}

/// What one target path observed, and how the trial should end because of it.
///
/// The target paths return this instead of persisting a terminal status, so the
/// single finalizer owns the evaluate-then-confirm-then-complete order.
pub(super) struct TrialTargetOutcome {
    /// Typed terminal evidence the deterministic evaluators read.
    pub(super) evidence: TrialTerminalEvidence,
    /// Status the trial reaches once its evidence is durable and visible.
    pub(super) terminal_status: ExperimentTrialStatus,
    /// Durable stop reason recorded with the terminal status.
    pub(super) stop_reason: ExperimentTrialStopReason,
    /// Terminal error message for failed trials.
    pub(super) error: Option<String>,
}

/// Running token, cost, turn, and visible-output observations for one trial.
#[derive(Debug, Default, Clone)]
struct TargetObservations {
    total_tokens: u64,
    total_cost_cents: u64,
    turns: u32,
    latest_output: Option<String>,
    latest_sequence: u64,
}

impl TargetObservations {
    fn absorb(&mut self, usage: &TargetUsageObservation) {
        self.total_tokens = self.total_tokens.saturating_add(usage.tokens);
        self.total_cost_cents = self.total_cost_cents.saturating_add(usage.cost_cents);
        self.latest_sequence = self.latest_sequence.max(usage.latest_sequence);
        if let Some(response) = usage.latest_response.clone() {
            self.latest_output = Some(response);
        }
    }

    fn into_outcome(
        self,
        target: TrialScoreTarget,
        session_id: SessionId,
        status: ExperimentTrialStatus,
        stop_reason: ExperimentTrialStopReason,
        error: Option<String>,
    ) -> TrialTargetOutcome {
        let outcome = match status {
            ExperimentTrialStatus::Completed => TrialTerminalOutcome::Completed,
            ExperimentTrialStatus::Cancelled => TrialTerminalOutcome::Cancelled,
            _ => failure_outcome(error.as_deref()),
        };
        TrialTargetOutcome {
            evidence: TrialTerminalEvidence {
                target,
                session_id,
                outcome,
                stop_reason,
                turn_count: self.turns,
                total_tokens: self.total_tokens,
                total_cost_cents: self.total_cost_cents,
                latest_sequence_num: self.latest_sequence,
                visible_output: self.latest_output,
                failure_code: error.as_ref().map(|_| outcome.as_str().to_string()),
            },
            terminal_status: status,
            stop_reason,
            error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkflowTrialStop {
    status: ExperimentTrialStatus,
    stop_reason: ExperimentTrialStopReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EffectiveExecutionSession {
    session_id: SessionId,
    contact_id: Option<ContactId>,
    target_session_supplied: bool,
}

struct CompiledExperimentTemplate {
    compiled: Option<moa_execution::CompiledExecution>,
    run_input: Value,
    audit: ExecutionPlanningAuditEnvelope,
    source_provenance: ExecutionSourceProvenance,
}

pub(super) async fn run_agent_loop_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    trial: ExperimentTrialRecord,
    simulator_context: SimulatorContext,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
    providers: &Arc<ProviderRegistry>,
) -> Result<TrialTargetOutcome, HandlerError> {
    // Scope mismatch is decided before the target payload is used for any
    // session read, session write, or simulator provider call.
    if trial.scope.tenant_id() != request.tenant_id {
        return Err(TerminalError::new_with_code(
            409,
            "experiment trial scope does not match the workflow tenant",
        )
        .into());
    }
    let target = parse_payload::<ExperimentTarget>("target", request.target.clone())?;
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let (session_id, target_model) =
        ensure_agent_loop_session(ctx, &request, &trial, target, variant, pool, session_store)
            .await?;
    ctx.set(K_SESSION_ID, Json(session_id));
    tracing::Span::current().set_attribute("moa.experiment.session_id", session_id.to_string());
    let score_target = TrialScoreTarget::Session { session_id };

    let initial_events =
        load_session_events(ctx, session_id, EventRange::all(), session_store).await?;
    let mut transcript = transcript_from_events(&initial_events);
    let mut transcript_sequence = latest_sequence(&initial_events);
    let mut target_usage_sequence = transcript_sequence;
    let usage_start_sequence = target_usage_start_sequence(ctx, transcript_sequence).await?;
    let (prior_tokens, prior_cost_cents) =
        target_usage_from_events_after(&initial_events, usage_start_sequence);
    // Token and cost observations were previously emitted as telemetry and then
    // discarded. The budget evaluators read them, so they are accumulated here
    // and travel with the evidence instead.
    let mut observations = TargetObservations {
        turns: trial.turn_count.max(0) as u32,
        total_tokens: prior_tokens,
        total_cost_cents: prior_cost_cents,
        latest_output: latest_brain_response(&initial_events),
        latest_sequence: transcript_sequence,
    };
    if forward_pending_child_cancellation(ctx, &trial.scope, trial.run_uid, pool).await? {
        return Ok(observations.into_outcome(
            score_target,
            session_id,
            ExperimentTrialStatus::Cancelled,
            ExperimentTrialStopReason::Cancelled,
            None,
        ));
    }
    for turn_index in trial.turn_count.max(0) as u32..simulator_context.max_turns {
        let observation = observe_session_after(
            ctx,
            &request.identity,
            session_id,
            transcript_sequence,
            session_store,
        )
        .await?;
        if let Some(stop) = stop_for_session_status(&observation.status) {
            return Ok(observations.into_outcome(
                score_target,
                session_id,
                stop.0,
                stop.1,
                terminal_status_error(stop.0, session_id),
            ));
        }
        if let Some(response) = observation.latest_response {
            observations.latest_output = Some(response.clone());
            transcript.push(ContextMessage::assistant(format!(
                "Target response: {response}"
            )));
        }
        transcript_sequence = observation.latest_sequence;
        observations.latest_sequence = observations.latest_sequence.max(transcript_sequence);

        let simulator_message = simulator_next_user_message(
            ctx,
            &trial,
            &simulator_context,
            &transcript,
            turn_index,
            providers,
        )
        .await?;
        if simulator_done(&simulator_message) {
            return Ok(observations.into_outcome(
                score_target,
                session_id,
                ExperimentTrialStatus::Completed,
                ExperimentTrialStopReason::SimulatorDone,
                None,
            ));
        }

        let response = with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .queue_message(Json::from(QueueMessageRequest {
                    // The trial uid plus this turn's index is the message's stable
                    // durable coordinate, so a replay of the trial workflow resubmits the
                    // same identity rather than starting a second simulated turn.
                    client_message_id: ClientMessageId::internal(
                        "experiment-trial",
                        trial.trial_uid,
                        u64::from(turn_index),
                    )
                    .map_err(moa_error_to_handler_error)?,
                    reply_to: None,
                    stream_cursor: None,
                    user_message: simulator_message.clone(),
                    attachments: Vec::new(),
                    model: target_model.as_ref().map(ToString::to_string),
                    contact: None,
                    max_turns: None,
                    execution_template: None,
                })),
            &request.identity,
        )
        .call()
        .await?
        .into_inner();
        let Some(turn_id) = response.started_turn_id else {
            return Err(TerminalError::new(
                "target session queued simulator message behind an active turn",
            )
            .into());
        };
        increment_trial_turn(ctx, trial.scope, trial.trial_uid, pool).await?;
        observations.turns = observations.turns.saturating_add(1);
        transcript.push(ContextMessage::user(simulator_message));

        let status =
            wait_for_target_after_turn(ctx, &request.identity, session_id, turn_id).await?;
        let usage =
            record_target_usage_after(ctx, session_id, &mut target_usage_sequence, session_store)
                .await?;
        observations.absorb(&usage);
        if let Some(stop) = stop_for_session_status(&status) {
            return Ok(observations.into_outcome(
                score_target,
                session_id,
                stop.0,
                stop.1,
                terminal_status_error(stop.0, session_id),
            ));
        }
    }

    Ok(observations.into_outcome(
        score_target,
        session_id,
        ExperimentTrialStatus::Completed,
        ExperimentTrialStopReason::MaxTurns,
        None,
    ))
}

async fn target_usage_start_sequence(
    ctx: &WorkflowContext<'_>,
    current_sequence: u64,
) -> Result<u64, HandlerError> {
    if let Some(sequence) = ctx
        .get::<Json<u64>>(K_TARGET_USAGE_START_SEQUENCE)
        .await?
        .map(Json::into_inner)
    {
        return Ok(sequence);
    }
    ctx.set(K_TARGET_USAGE_START_SEQUENCE, Json(current_sequence));
    Ok(current_sequence)
}

/// Returns a stable, PII-free error string for a non-clean target stop.
fn terminal_status_error(status: ExperimentTrialStatus, session_id: SessionId) -> Option<String> {
    matches!(status, ExperimentTrialStatus::Failed)
        .then(|| format!("target session {session_id} reached a failed state"))
}

/// Agent and model selection for one agent-loop trial target.
#[derive(Debug, Clone, PartialEq)]
struct AgentLoopTargetSelection {
    /// Agent selector used when the trial creates its own session.
    agent: Option<AgentSessionSelection>,
    /// Effective target model, most specific source first.
    model: Option<ModelId>,
}

/// Validates an agent-loop trial target and resolves its agent and model.
///
/// A simulator trial never continues a caller-named session: it would submit
/// live turns into a production conversation and read that conversation's
/// durable event log. The target type carries no session field, so the trial
/// always runs in the eval-owned session it creates for itself.
fn agent_loop_target_selection(
    target: ExperimentTarget,
    trial_model: Option<ModelId>,
    variant_model: Option<ModelId>,
) -> Result<AgentLoopTargetSelection, HandlerError> {
    let ExperimentTarget::AgentLoop {
        agent,
        model,
        attachments,
        ..
    } = target
    else {
        return Err(bad_request(
            "agent-loop trial received a workflow experiment target",
        ));
    };
    if !attachments.is_empty() {
        return Err(bad_request(
            "simulator trials do not copy target prompt attachments into simulator turns",
        ));
    }
    Ok(AgentLoopTargetSelection {
        agent,
        model: trial_model.or(variant_model).or(Some(model)),
    })
}

/// Refuses a resumed trial session that does not belong to the trial's scope.
fn require_trial_session_ownership(
    meta: &SessionMeta,
    tenant_id: TenantId,
    scope: ActionRuleScope,
) -> Result<(), HandlerError> {
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    // Agent-loop trial sessions are created without a contact, so a contact is
    // only compared when the resumed session actually declares one.
    if meta.tenant_id != tenant_id
        || (contact_id.is_some() && contact_id != scope.contact_id())
        || meta.tenant_id != scope.tenant_id()
    {
        return Err(TerminalError::new_with_code(
            409,
            "agent-loop trial session does not match the experiment trial scope",
        )
        .into());
    }
    Ok(())
}

/// Authorizes and verifies a resumed trial session before its first event read.
///
/// `Session/status` performs `require_authz_with_delegation(Session, id,
/// Participant)`. Calling it here puts the authorization and the ownership
/// check ahead of every durable event read the trial performs, instead of
/// leaving the first read in front of the first authorized call.
async fn authorize_resumed_trial_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
    trial: &ExperimentTrialRecord,
    session_id: SessionId,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(), HandlerError> {
    with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .status(),
        &request.identity,
    )
    .call()
    .await?;
    let store = session_store.clone();
    let meta = ctx
        .run(|| async move {
            store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("experiment_trial_load_agent_loop_session")
        .await?
        .into_inner();
    require_trial_session_ownership(&meta, request.tenant_id, trial.scope)
}

async fn ensure_agent_loop_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
    trial: &ExperimentTrialRecord,
    target: ExperimentTarget,
    variant: ExperimentVariant,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(SessionId, Option<ModelId>), HandlerError> {
    let selection = agent_loop_target_selection(target, trial.target_model.clone(), variant.model)?;
    let target_model = selection.model;

    let scope = trial.scope;
    let session_id = match trial.session_id {
        // A trial that already attached a session is resuming its own eval-owned
        // session. It is still authorized and ownership-checked before this
        // workflow reads or writes anything through it.
        Some(session_id) => {
            authorize_resumed_trial_session(ctx, request, trial, session_id, session_store).await?;
            session_id
        }
        None => {
            let model = target_model
                .clone()
                .ok_or_else(|| bad_request("agent-loop trial requires a target model"))?;
            let agent = selection.agent.ok_or_else(|| {
                bad_request("agent-loop simulator target requires an agent selector")
            })?;
            let (session_id, meta) = create_new_session(
                ctx,
                request.tenant_id,
                model,
                &request.identity,
                trial_call_origin(trial),
                agent,
                pool,
                session_store,
            )
            .await?;
            with_identity_headers(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .set_meta(Json::from(meta)),
                &request.identity,
            )
            .call()
            .await?;
            session_id
        }
    };
    attach_trial_session(ctx, scope, trial.trial_uid, session_id, pool).await?;
    Ok((session_id, target_model))
}

/// Executes one pinned execution-template trial through typed Execution services.
pub(super) async fn run_execution_template_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    trial: ExperimentTrialRecord,
    config: &MoaConfig,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<TrialTargetOutcome, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target.clone())?;
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let ExperimentTarget::ExecutionTemplate {
        template,
        objective,
        input,
        session_id: target_session_id,
        idempotency_key,
    } = target
    else {
        return Err(bad_request(
            "execution-template trial received an agent-loop experiment target",
        ));
    };
    if objective.trim().is_empty() {
        return Err(bad_request(
            "execution-template trial objective must not be empty",
        ));
    }
    if variant.execution_template.as_ref() != Some(&template) {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template target and variant do not pin the same revision",
        )
        .into());
    }
    if trial.scope.tenant_id() != request.tenant_id {
        return Err(TerminalError::new_with_code(
            409,
            "experiment trial scope does not match the workflow tenant",
        )
        .into());
    }

    let effective = ensure_execution_session(
        ctx,
        &request,
        &trial,
        &variant,
        target_session_id,
        config,
        pool,
        session_store,
    )
    .await?;
    ctx.set(K_SESSION_ID, Json(effective.session_id));
    if let Some(contact_id) = effective.contact_id {
        ctx.set(K_EXECUTION_CONTACT_ID, Json(contact_id));
    }
    tracing::Span::current().set_attribute(
        "moa.experiment.session_id",
        effective.session_id.to_string(),
    );
    attach_trial_session(
        ctx,
        trial.scope,
        trial.trial_uid,
        effective.session_id,
        pool,
    )
    .await?;
    if forward_pending_child_cancellation(ctx, &trial.scope, trial.run_uid, pool).await? {
        return Ok(TargetObservations {
            turns: trial.turn_count.max(0) as u32,
            ..TargetObservations::default()
        }
        .into_outcome(
            TrialScoreTarget::Session {
                session_id: effective.session_id,
            },
            effective.session_id,
            ExperimentTrialStatus::Cancelled,
            ExperimentTrialStopReason::Cancelled,
            None,
        ));
    }

    let origin = append_experiment_objective(
        ctx,
        effective.session_id,
        &objective,
        trial.run_uid,
        trial.score_run_id,
        trial.trial_uid,
    )
    .await?;
    let planning_call = ctx
        .service_client::<ExecutionClient>()
        .planning_context(Json::from(ExecutionPlanningContextRequest {
            tenant_id: request.tenant_id,
            contact_id: effective.contact_id,
            session_id: effective.session_id,
            originating_user_sequence_num: origin.sequence_num,
            requested_template: Some(template.clone()),
        }));
    let planning_context = with_identity_headers(planning_call, &request.identity)
        .call()
        .await?
        .into_inner();
    let operation_key =
        experiment_trial_operation_key(trial.run_uid, trial.score_run_id, trial.trial_uid);
    let now = durable_utc_now(ctx, "experiment_trial_execution_compile_now").await?;
    let compiled = compile_experiment_template(ExperimentTemplateCompileRequest {
        context: &planning_context.snapshot,
        requested: &template,
        config,
        objective,
        input,
        experiment_run_uid: trial.run_uid,
        score_run_id: trial.score_run_id,
        trial_uid: trial.trial_uid,
        operation_key,
        now,
    })?;
    persist_compile_audit(ctx, trial.scope, compiled.audit, pool).await?;
    let compiled_plan = compiled.compiled.ok_or_else(|| {
        TerminalError::new_with_code(422, "experiment execution template was rejected")
    })?;

    let start_call =
        ctx.service_client::<ExecutionClient>()
            .start(Json::from(ExecutionStartRequest {
                tenant_id: request.tenant_id,
                contact_id: effective.contact_id,
                session_id: effective.session_id,
                originating_user_sequence_num: origin.sequence_num,
                planning_context_uid: planning_context.planning_context_uid,
                planning_context_hash: planning_context.planning_context_hash,
                idempotency_key: idempotency_key.or_else(|| {
                    Some(format!(
                        "experiment-trial:{}:{}:{}",
                        trial.run_uid, trial.score_run_id, trial.trial_uid
                    ))
                }),
                compiled: compiled_plan,
                run_input: compiled.run_input,
                source_provenance: compiled.source_provenance,
            }));
    let started = with_identity_headers(start_call, &request.identity)
        .call()
        .await?
        .into_inner();
    let execution_run_uid = started.run.run_uid;
    let attach_pool = pool.clone();
    let scope = trial.scope;
    let trial_uid = trial.trial_uid;
    ctx.run(|| async move {
        attach_trial_execution_run(attach_pool, scope, trial_uid, execution_run_uid)
            .await
            .map(Json::from)
    })
    .name("experiment_trial_attach_execution_run")
    .await?;
    ctx.set(K_EXECUTION_RUN_UID, Json(execution_run_uid));
    forward_pending_child_cancellation(ctx, &trial.scope, trial.run_uid, pool).await?;
    tracing::Span::current().set_attribute(
        "moa.experiment.execution_run_uid",
        execution_run_uid.to_string(),
    );

    let (stop, error) = wait_for_execution_outcome(
        ctx,
        &request.identity,
        request.tenant_id,
        effective.contact_id,
        effective.session_id,
        execution_run_uid,
    )
    .await?;
    // The typed execution path never observed its own token or cost use; it only
    // polled for a terminal status. The budget evaluators need real numbers, so
    // the durable session log is read once here rather than left as a gap the
    // evaluators would have to guess around.
    let mut sequence = 0_u64;
    let usage =
        record_target_usage_after(ctx, effective.session_id, &mut sequence, session_store).await?;
    let mut observations = TargetObservations {
        turns: trial.turn_count.max(0) as u32,
        ..TargetObservations::default()
    };
    observations.absorb(&usage);

    Ok(observations.into_outcome(
        TrialScoreTarget::ExecutionRun { execution_run_uid },
        effective.session_id,
        stop.status,
        stop.stop_reason,
        error,
    ))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
async fn ensure_execution_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
    trial: &ExperimentTrialRecord,
    variant: &ExperimentVariant,
    target_session_id: Option<SessionId>,
    config: &MoaConfig,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<EffectiveExecutionSession, HandlerError> {
    if let Some(session_id) = target_session_id {
        with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .status(),
            &request.identity,
        )
        .call()
        .await?;
        let store = session_store.clone();
        let meta = ctx
            .run(|| async move {
                store
                    .get_session(session_id)
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            })
            .name("experiment_trial_load_execution_session")
            .await?
            .into_inner();
        let contact_id = admit_caller_named_execution_session(
            &meta,
            request.tenant_id,
            trial.scope,
            trial_call_origin(trial),
        )?;
        return Ok(EffectiveExecutionSession {
            session_id,
            contact_id,
            target_session_supplied: true,
        });
    }

    let session_id = experiment_execution_session_id(
        request.tenant_id,
        trial.run_uid,
        trial.score_run_id,
        Some(trial.trial_uid),
    )?;
    let model = trial
        .target_model
        .clone()
        .or_else(|| variant.model.clone())
        .unwrap_or_else(|| ModelId::new(config.models.main.clone()));
    let now = durable_utc_now(ctx, "experiment_trial_internal_session_now").await?;
    let meta = internal_execution_session_meta(
        session_id,
        trial.scope,
        model,
        now,
        &request.identity,
        trial_call_origin(trial),
    )?;
    let store = session_store.clone();
    let init_pool = pool.clone();
    let init_meta = meta.clone();
    let identity = request.identity.clone();
    let fga = crate::handlers::authz_shim::require_fga_client()?;
    let initialized = ctx
        .run(|| async move {
            let initialized = crate::services::session_store::inner::initialize_internal_execution_session_atomic(
                store.as_ref(),
                &init_pool,
                init_meta,
                identity.clone(),
            )
            .await?;
            crate::services::session_store::inner::ensure_session_authz_visible(
                &init_pool,
                &fga,
                &identity,
                initialized,
            )
            .await?;
            Ok::<_, HandlerError>(Json::from(initialized))
        })
        .name("experiment_trial_initialize_internal_execution_session")
        .await?
        .into_inner();
    if initialized != session_id {
        return Err(TerminalError::new_with_code(
            409,
            "internal experiment Session initialization returned a different key",
        )
        .into());
    }
    crate::restate_identity::replay_safe_request(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .set_meta(Json::from(meta)),
    )
    .call()
    .await?;
    Ok(EffectiveExecutionSession {
        session_id,
        contact_id: trial.scope.contact_id(),
        target_session_supplied: false,
    })
}

/// Admits one caller-named execution Session, or refuses it before it is used.
///
/// A caller names this Session in the experiment target, so nothing about it is
/// derived from the trial. Tenant and contact scope alone are not enough: an
/// ordinary production Session of the same tenant would pass them, and a
/// production [`CallOrigin`] composes with the process-wide router's own
/// production origin to leave every tool this trial's tasks issue holding the
/// full production capability set — production connectors and side-effecting
/// host tools included.
///
/// So the named Session must carry exactly the origin this trial stamps on the
/// Session it would otherwise create for itself: the same run uid, and this
/// trial's own uid. Requiring the run uid stops one eval run from borrowing
/// another's Session; requiring this trial's uid stops a sibling trial's
/// Session — or the run's own trial-less Session — from hosting this trial,
/// which would attribute its refusals to a unit that is not executing.
///
/// Returns the contact the admitted Session is scoped to.
fn admit_caller_named_execution_session(
    meta: &SessionMeta,
    tenant_id: TenantId,
    scope: ActionRuleScope,
    expected_origin: CallOrigin,
) -> Result<Option<ContactId>, HandlerError> {
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    if meta.tenant_id != tenant_id
        || meta.tenant_id != scope.tenant_id()
        || contact_id != scope.contact_id()
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template target Session does not match experiment scope",
        )
        .into());
    }
    if meta.call_origin != expected_origin {
        return Err(TerminalError::new_with_code(
            409,
            format!(
                "execution-template target Session carries a {} call origin instead of this \
                 experiment trial's own eval-owned origin",
                meta.call_origin.as_str()
            ),
        )
        .into());
    }
    Ok(contact_id)
}

/// Returns the eval-owned call origin of one trial.
///
/// Both trial target kinds are stamped from here, so an execution-template
/// trial and an agent-loop trial cannot end up with different ceilings. The
/// trial uid is always present because a trial exists.
fn trial_call_origin(trial: &ExperimentTrialRecord) -> CallOrigin {
    CallOrigin::Experiment {
        run_uid: trial.run_uid,
        trial_uid: Some(trial.trial_uid),
    }
}

/// Builds the metadata for one execution-template trial's internal session.
///
/// Stamped with the same eval-owned [`CallOrigin`] as the agent-loop path: the
/// execution run this session hosts dispatches its task tools through the same
/// process-wide router, and the tool executor reloads this record to decide
/// what those tasks may hold.
fn internal_execution_session_meta(
    session_id: SessionId,
    scope: ActionRuleScope,
    model: ModelId,
    now: chrono::DateTime<Utc>,
    identity: &Identity,
    call_origin: CallOrigin,
) -> Result<SessionMeta, HandlerError> {
    let contact = scope.contact_id().map(|contact_id| ContactRef {
        contact_id,
        tenant_id: scope.tenant_id(),
        state: ContactVerificationState::Unverified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    });
    Ok(SessionMeta {
        id: session_id,
        tenant_id: scope.tenant_id(),
        title: Some("Experiment execution-template trial".to_string()),
        status: SessionStatus::Created,
        channel: Channel::Chat,
        model,
        created_at: now,
        updated_at: now,
        created_by: Some(session_actor_ref(identity)?),
        contact,
        agent_context: Some(AgentContext::system_default()),
        call_origin,
        ..SessionMeta::default()
    })
}

fn experiment_execution_session_id(
    tenant_id: TenantId,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Option<Uuid>,
) -> Result<SessionId, HandlerError> {
    let mut name = EXPERIMENT_EXECUTION_SESSION_DOMAIN.as_bytes().to_vec();
    append_nullable_frame(&mut name, Some(tenant_id.to_string().as_bytes()))?;
    append_nullable_frame(&mut name, Some(experiment_run_uid.to_string().as_bytes()))?;
    append_nullable_frame(&mut name, Some(score_run_id.to_string().as_bytes()))?;
    let trial_uid = trial_uid.map(|value| value.to_string());
    append_nullable_frame(&mut name, trial_uid.as_deref().map(str::as_bytes))?;
    Ok(SessionId(Uuid::new_v5(
        &EXPERIMENT_EXECUTION_SESSION_NAMESPACE,
        &name,
    )))
}

fn append_nullable_frame(output: &mut Vec<u8>, value: Option<&[u8]>) -> Result<(), HandlerError> {
    let Some(value) = value else {
        output.push(0);
        return Ok(());
    };
    output.push(1);
    let length = u32::try_from(value.len()).map_err(|_| {
        TerminalError::new("experiment execution Session identity field exceeds framing")
    })?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

async fn append_experiment_objective(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    objective: &str,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Uuid,
) -> Result<EventRecord, HandlerError> {
    let event = Event::UserMessage {
        text: objective.to_string(),
        attachments: Vec::new(),
    };
    let persisted = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: event.clone(),
                dedupe_key: Some(format!(
                    "experiment-objective:{experiment_run_uid}:{score_run_id}:{trial_uid}"
                )),
            })),
    )
    .call()
    .await?
    .into_inner();
    if persisted.event != event {
        return Err(TerminalError::new_with_code(
            409,
            "experiment objective replay conflicts with the first persisted event",
        )
        .into());
    }
    Ok(persisted)
}

fn experiment_trial_operation_key(
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Uuid,
) -> String {
    format!("experiment:{experiment_run_uid}:{score_run_id}:{trial_uid}")
}

#[derive(Serialize)]
struct ExperimentCompileCandidate<'a> {
    kind: &'static str,
    schema_version: u8,
    source: ExecutionCompileSource,
    goal: &'a ExecutionGoalContract,
    plan: &'a moa_artifacts::execution_plan::ExecutionPlanDefinition,
    run_input: &'a Value,
}

#[derive(Clone, Copy)]
enum ExperimentCompileClassification {
    Accepted,
    NeedsInput,
    Unsupported,
    Rejected,
}

struct ExperimentTemplateCompileRequest<'a> {
    context: &'a moa_execution::wire::ExecutionPlanningContextSnapshot,
    requested: &'a PinnedExecutionTemplateRef,
    config: &'a MoaConfig,
    objective: String,
    input: Value,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Uuid,
    operation_key: String,
    now: chrono::DateTime<Utc>,
}

fn compile_experiment_template(
    request: ExperimentTemplateCompileRequest<'_>,
) -> Result<CompiledExperimentTemplate, HandlerError> {
    let ExperimentTemplateCompileRequest {
        context,
        requested,
        config,
        objective,
        input,
        experiment_run_uid,
        score_run_id,
        trial_uid,
        operation_key,
        now,
    } = request;
    let parsed = ArtifactRef::from_str(&requested.skill_ref)
        .map_err(|error| bad_request(format!("invalid execution template ref: {error}")))?;
    if parsed
        .canonical_string()
        .map_err(|error| bad_request(format!("invalid execution template ref: {error}")))?
        != requested.skill_ref
    {
        return Err(bad_request("execution template ref must be canonical"));
    }
    let mut matching = context.execution_templates.iter().filter(|template| {
        template.skill_ref == parsed && template.revision_uid == requested.revision_uid
    });
    let template = matching.next().ok_or_else(|| {
        TerminalError::new_with_code(
            422,
            "requested execution template is not pinned in the planning context",
        )
    })?;
    if matching.next().is_some() {
        return Err(TerminalError::new_with_code(
            409,
            "requested execution template is duplicated in the planning context",
        )
        .into());
    }
    validate_instance(&template.skill_input_schema, &input, "skill_input_schema")
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;

    let candidate = GeneratedExecutionCandidate {
        goal: template.execution_plan.instantiate_goal(objective),
        plan: template.execution_plan.plan.clone(),
        run_input: input,
    };
    let candidate_preimage = ExperimentCompileCandidate {
        kind: "initial",
        schema_version: 1,
        source: ExecutionCompileSource::ExperimentTemplate,
        goal: &candidate.goal,
        plan: &candidate.plan,
        run_input: &candidate.run_input,
    };
    let candidate_bytes = artifact_canonical_json_bytes(&candidate_preimage)
        .map_err(|error| TerminalError::new(error.to_string()))?;
    let candidate_hash =
        execution_planning_hash("moa.execution.compile-candidate", &candidate_bytes);
    let started_at = Instant::now();
    let outcome = compile(CompileExecutionRequest {
        goal: candidate.goal,
        plan: candidate.plan,
        run_input: candidate.run_input.clone(),
        catalog: context.catalog.clone(),
        authorization: context.authorization.clone(),
        approved_budget: context.budget.clone(),
        config: config.execution.clone(),
        now,
    });
    let duration_micros = u64::try_from(started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
    let classification = classify_experiment_compile(&outcome);
    let report = compiler_audit_report(&outcome.report)?;
    let report_bytes =
        canonical_json_bytes(&report).map_err(|error| TerminalError::new(error.to_string()))?;
    let validation_report =
        String::from_utf8(report_bytes).map_err(|error| TerminalError::new(error.to_string()))?;
    let compile_outcome = match classification {
        ExperimentCompileClassification::Accepted => ExecutionCompileOutcome::Accepted,
        ExperimentCompileClassification::NeedsInput => ExecutionCompileOutcome::NeedsInput,
        ExperimentCompileClassification::Unsupported => ExecutionCompileOutcome::Unsupported,
        ExperimentCompileClassification::Rejected => ExecutionCompileOutcome::Rejected,
    };
    let final_plan_hash = outcome
        .compiled
        .as_ref()
        .map(|compiled| compiled.plan.plan_hash.to_string());
    let canonical_ref = template
        .skill_ref
        .canonical_string()
        .map_err(|error| TerminalError::new(error.to_string()))?;
    Ok(CompiledExperimentTemplate {
        compiled: outcome.compiled,
        run_input: candidate.run_input,
        audit: ExecutionPlanningAuditEnvelope {
            schema_version: 1,
            tenant_id: context.tenant_id,
            contact_id: context.contact_id,
            session_id: Some(context.session_id),
            originating_sequence: Some(context.originating_user_sequence_num),
            payload: ExecutionPlanningAuditPayload::Compile {
                source: ExecutionCompileSource::ExperimentTemplate,
                operation_key,
                run_uid: None,
                plan_revision: None,
                outcome: compile_outcome,
                candidate_hash,
                final_plan_hash,
                validation_report,
                duration_micros,
                created_at: now,
            },
        },
        source_provenance: experiment_template_source_provenance(
            canonical_ref,
            template.revision_uid,
            experiment_run_uid,
            score_run_id,
            trial_uid,
        ),
    })
}

fn experiment_template_source_provenance(
    skill_template_ref: String,
    skill_template_revision_uid: Uuid,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
    trial_uid: Uuid,
) -> ExecutionSourceProvenance {
    ExecutionSourceProvenance::ExperimentTemplate {
        skill_template_ref,
        skill_template_revision_uid,
        experiment_run_uid,
        score_run_id,
        trial_uid: Some(trial_uid),
    }
}

fn classify_experiment_compile(
    outcome: &CompileExecutionOutcome,
) -> ExperimentCompileClassification {
    if outcome.compiled.is_some() && !outcome.report.has_errors() {
        return ExperimentCompileClassification::Accepted;
    }
    let error_codes = outcome
        .report
        .issues
        .iter()
        .filter(|issue| issue.severity == ExecutionValidationSeverity::Error)
        .map(|issue| issue.code.as_str())
        .collect::<Vec<_>>();
    if error_codes.iter().any(|code| {
        matches!(
            *code,
            "invalid_run_input" | "empty_objective" | "goal_structure"
        )
    }) {
        ExperimentCompileClassification::NeedsInput
    } else if error_codes.iter().any(|code| {
        code.contains("authorization")
            || code.contains("capability")
            || code.contains("budget")
            || code.contains("deadline")
            || code.starts_with("unsupported_")
            || *code == "skill_not_authorized"
            || *code == "objective_mismatch"
    }) {
        ExperimentCompileClassification::Unsupported
    } else {
        ExperimentCompileClassification::Rejected
    }
}

fn compiler_audit_report(
    report: &ExecutionValidationReport,
) -> Result<moa_core::types::execution_planning::ExecutionAuditReport, HandlerError> {
    let violations = report
        .issues
        .iter()
        .map(|issue| ExecutionAuditViolation {
            code: issue.code.clone(),
            path: issue.path.clone(),
            message: issue.message.clone(),
        })
        .collect();
    bounded_audit_report(true, violations)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()).into())
}

async fn persist_compile_audit(
    ctx: &WorkflowContext<'_>,
    scope: ActionRuleScope,
    audit: ExecutionPlanningAuditEnvelope,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let execution_scope = match scope {
        ActionRuleScope::Tenant { tenant_id } => ExecutionScope::Tenant { tenant_id },
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => ExecutionScope::Contact {
            tenant_id,
            contact_id,
        },
    };
    let audit_pool = pool.clone();
    let outcome = ctx
        .run(|| async move {
            ExecutionRepository::new(audit_pool)
                .write_compile_audit(execution_scope, &audit)
                .await
                .map(Json::from)
                .map_err(|error| {
                    TerminalError::new(format!(
                        "experiment trial compile audit persistence failed: {error}"
                    ))
                    .into()
                })
        })
        .name("experiment_trial_write_compile_audit")
        .await?
        .into_inner();
    moa_brain::execution_planning::request::record_applied_planning_audit(&outcome);
    if matches!(outcome, CompileAuditWriteOutcome::Conflict { .. }) {
        return Err(TerminalError::new_with_code(
            409,
            "experiment trial compile audit conflicts with first persisted evidence",
        )
        .into());
    }
    Ok(())
}

async fn wait_for_execution_outcome(
    ctx: &WorkflowContext<'_>,
    identity: &Identity,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    session_id: SessionId,
    run_uid: Uuid,
) -> Result<(WorkflowTrialStop, Option<String>), HandlerError> {
    let run = ExecutionRunRequest {
        tenant_id,
        contact_id,
        session_id,
        run_uid,
    };
    let poll_count =
        EXECUTION_TARGET_WAIT_TIMEOUT.as_secs() / EXECUTION_STATUS_POLL_INTERVAL.as_secs();
    for _ in 0..poll_count {
        let status = with_identity_headers(
            ctx.service_client::<ExecutionClient>()
                .status(Json::from(run.clone())),
            identity,
        )
        .call()
        .await?
        .into_inner();
        if let Some(terminal) = trial_stop_for_execution_status(&status) {
            return Ok(terminal);
        }
        ctx.sleep(EXECUTION_STATUS_POLL_INTERVAL).await?;
    }
    let reason = format!(
        "experiment trial timed out waiting {EXECUTION_TARGET_WAIT_TIMEOUT:?} for execution run {run_uid}"
    );
    with_identity_headers(
        ctx.service_client::<ExecutionClient>()
            .cancel(Json::from(ExecutionCancelRequest {
                run,
                reason: reason.clone(),
            })),
        identity,
    )
    .call()
    .await?;
    Ok((execution_failure_stop(), Some(reason)))
}

fn trial_stop_for_execution_status(
    response: &ExecutionStatusResponse,
) -> Option<(WorkflowTrialStop, Option<String>)> {
    let stop = trial_stop_for_execution_run_status(response.run.status)?;
    let error = matches!(stop.status, ExperimentTrialStatus::Failed).then(|| {
        format!(
            "execution run {} ended with status {} and gaps {:?}",
            response.run.run_uid,
            response.run.status.as_str(),
            response.gaps
        )
    });
    Some((stop, error))
}

fn execution_failure_stop() -> WorkflowTrialStop {
    WorkflowTrialStop {
        status: ExperimentTrialStatus::Failed,
        stop_reason: ExperimentTrialStopReason::Error,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
    call_origin: CallOrigin,
    agent: AgentSessionSelection,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let prepare_identity = identity.clone();
    let prepare_pool = pool.clone();
    let meta = ctx
        .run(|| async move {
            let mut meta = new_session_meta(tenant_id, model, &prepare_identity, call_origin)?;
            let agent_context =
                resolve_agent_context_for_session(prepare_pool, &meta, &agent).await?;
            apply_agent_model_policy(&mut meta, &agent_context)?;
            meta.agent_context = Some(agent_context);
            Ok::<_, HandlerError>(Json::from(meta))
        })
        .name("experiment_trial_prepare_session")
        .await?
        .into_inner();
    let store = session_store.clone();
    let pool = pool.clone();
    let identity = identity.clone();
    let fga = crate::handlers::authz_shim::require_fga_client()?;
    Ok(ctx
        .run(|| async move {
            let session_id =
                create_session_for_identity(store.as_ref(), &pool, meta.clone(), identity.clone())
                    .await
                    .map_err(non_retryable_handler_error)?;
            crate::services::session_store::inner::ensure_session_authz_visible(
                &pool, &fga, &identity, session_id,
            )
            .await?;
            Ok::<_, HandlerError>(Json::from((session_id, meta)))
        })
        .name("experiment_trial_create_session")
        .await?
        .into_inner())
}

async fn observe_session_after(
    ctx: &WorkflowContext<'_>,
    identity: &Identity,
    session_id: SessionId,
    sequence_num: u64,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<TargetObservation, HandlerError> {
    let status = with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .status(),
        identity,
    )
    .call()
    .await?
    .into_inner();
    let events = load_session_events(
        ctx,
        session_id,
        event_range_after(sequence_num),
        session_store,
    )
    .await?;
    Ok(TargetObservation {
        status,
        latest_response: latest_brain_response(&events),
        latest_sequence: latest_sequence(&events).max(sequence_num),
    })
}

async fn load_session_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    range: EventRange,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = session_store.clone();
    Ok(ctx
        .run(|| async move {
            store
                .get_events(session_id, range)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("experiment_trial_load_session_events")
        .await?
        .into_inner())
}

async fn wait_for_target_after_turn(
    ctx: &WorkflowContext<'_>,
    identity: &Identity,
    session_id: SessionId,
    turn_id: String,
) -> Result<SessionStatus, HandlerError> {
    let (awakeable_id, completion) = ctx.awakeable::<String>();
    let attached = with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .attach_turn_waiter(Json::from(AttachSessionTurnWaiterInput {
                turn_id: turn_id.clone(),
                awakeable_id: awakeable_id.clone(),
            })),
        identity,
    )
    .call()
    .await?
    .into_inner();
    if let Some(outcome) = attached.outcome {
        return status_for_turn_outcome(&outcome);
    }

    restate_sdk::select! {
        outcome = completion => {
            let outcome = parse_turn_outcome(&outcome?)?;
            status_for_turn_outcome(&outcome)
        },
        _ = ctx.sleep(EXECUTION_TARGET_WAIT_TIMEOUT) => {
            with_identity_headers(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .remove_turn_waiter(Json::from(RemoveSessionTurnWaiterInput {
                    turn_id: turn_id.clone(),
                    awakeable_id,
                })),
                identity,
            )
            .call()
            .await?;
            Err(TerminalError::new(format!(
                "timed out waiting for target session turn {turn_id}"
            )).into())
        }
    }
}

async fn record_target_usage_after(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    sequence_num: &mut u64,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<TargetUsageObservation, HandlerError> {
    let store = session_store.clone();
    let range = event_range_after(*sequence_num);
    let previous_sequence = *sequence_num;
    let observation = ctx
        .run(|| async move {
            let events = store
                .get_events(session_id, range)
                .await
                .map_err(moa_error_to_handler_error)?;
            let (tokens, cost_cents) = target_usage_from_events(&events);
            record_simulation_tokens("target", tokens);
            record_simulation_cost_cents("target", cost_cents);
            Ok::<_, HandlerError>(Json::from(TargetUsageObservation {
                latest_sequence: latest_sequence(&events).max(previous_sequence),
                tokens,
                cost_cents,
                latest_response: latest_brain_response(&events),
            }))
        })
        .name("experiment_trial_record_target_usage")
        .await?
        .into_inner();
    *sequence_num = observation.latest_sequence;
    Ok(observation)
}

fn target_usage_from_events(events: &[EventRecord]) -> (u64, u64) {
    events
        .iter()
        .fold((0_u64, 0_u64), |(tokens, cost_cents), record| {
            (
                tokens + (record.event.input_tokens() + record.event.output_tokens()) as u64,
                cost_cents + u64::from(record.event.cost_cents()),
            )
        })
}

fn target_usage_from_events_after(events: &[EventRecord], boundary: u64) -> (u64, u64) {
    events
        .iter()
        .filter(|record| record.sequence_num > boundary)
        .fold((0_u64, 0_u64), |(tokens, cost_cents), record| {
            (
                tokens + (record.event.input_tokens() + record.event.output_tokens()) as u64,
                cost_cents + u64::from(record.event.cost_cents()),
            )
        })
}

fn event_range_after(sequence_num: u64) -> EventRange {
    EventRange {
        from_seq: Some(sequence_num.saturating_add(1)),
        event_types: Some(vec![EventType::UserMessage, EventType::BrainResponse]),
        ..EventRange::default()
    }
}

fn latest_sequence(events: &[EventRecord]) -> u64 {
    events
        .last()
        .map(|record| record.sequence_num)
        .unwrap_or_default()
}

fn transcript_from_events(events: &[EventRecord]) -> Vec<ContextMessage> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::UserMessage { text, .. } if !text.trim().is_empty() => {
                Some(ContextMessage::user(text.clone()))
            }
            Event::BrainResponse { text, .. } if !text.trim().is_empty() => Some(
                ContextMessage::assistant(format!("Target response: {text}")),
            ),
            _ => None,
        })
        .collect()
}

fn parse_turn_outcome(raw: &str) -> Result<TurnOutcome, HandlerError> {
    serde_json::from_str(raw).map_err(|error| {
        TerminalError::new(format!(
            "failed to deserialize target turn outcome: {error}"
        ))
        .into()
    })
}

fn status_for_turn_outcome(outcome: &TurnOutcome) -> Result<SessionStatus, HandlerError> {
    Ok(match outcome.kind {
        TurnOutcomeKind::Completed => SessionStatus::Paused,
        TurnOutcomeKind::Cancelled => SessionStatus::Cancelled,
        TurnOutcomeKind::Failed => SessionStatus::Failed,
        TurnOutcomeKind::Accepted { .. } => {
            return Err(
                TerminalError::new_with_code(409, "run_requires_user_message_origin").into(),
            );
        }
    })
}

fn latest_brain_response(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

pub(super) fn stop_for_session_status(
    status: &SessionStatus,
) -> Option<(ExperimentTrialStatus, ExperimentTrialStopReason)> {
    match status {
        SessionStatus::Completed => Some((
            ExperimentTrialStatus::Completed,
            ExperimentTrialStopReason::TargetTerminal,
        )),
        SessionStatus::Cancelled => Some((
            ExperimentTrialStatus::Cancelled,
            ExperimentTrialStopReason::Cancelled,
        )),
        SessionStatus::Failed => Some((
            ExperimentTrialStatus::Failed,
            ExperimentTrialStopReason::Error,
        )),
        SessionStatus::Created | SessionStatus::Running | SessionStatus::Paused => None,
    }
}

fn trial_stop_for_execution_run_status(status: ExecutionRunStatus) -> Option<WorkflowTrialStop> {
    match status {
        ExecutionRunStatus::Completed => Some(WorkflowTrialStop {
            status: ExperimentTrialStatus::Completed,
            stop_reason: ExperimentTrialStopReason::TargetTerminal,
        }),
        ExecutionRunStatus::Cancelled => Some(WorkflowTrialStop {
            status: ExperimentTrialStatus::Cancelled,
            stop_reason: ExperimentTrialStopReason::Cancelled,
        }),
        ExecutionRunStatus::Partial
        | ExecutionRunStatus::Blocked
        | ExecutionRunStatus::Unsupported
        | ExecutionRunStatus::Failed => Some(execution_failure_stop()),
        ExecutionRunStatus::AwaitingConfirmation
        | ExecutionRunStatus::Queued
        | ExecutionRunStatus::Running
        | ExecutionRunStatus::WaitingInput
        | ExecutionRunStatus::WaitingReview
        | ExecutionRunStatus::WaitingReplan => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::{
        types::context::MessageRole, types::events_stream::EventRecord, types::provider::ModelTier,
    };

    const TRIAL_CONNECTOR: &str = "crm";
    const TRIAL_CONNECTOR_TOOL: &str = "create_deal";

    /// Spawns a connector that answers discovery and flags any `tools/call`.
    ///
    /// The flag is the proof a refusal happened before the network, not after a
    /// side effect the trial already caused.
    async fn spawn_recording_connector() -> (String, Arc<std::sync::atomic::AtomicBool>) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake connector");
        let addr = listener.local_addr().expect("fake connector address");
        let tool_calls = Arc::new(AtomicBool::new(false));
        let seen_calls = Arc::clone(&tool_calls);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let bytes = match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => continue,
                    Ok(read) => read,
                };
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let method = request
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                let body = match method.as_deref() {
                    Some("initialize") => {
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#
                            .to_string()
                    }
                    Some("tools/list") => format!(
                        r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{TRIAL_CONNECTOR_TOOL}","description":"Create a CRM deal","inputSchema":{{"type":"object","properties":{{"account":{{"type":"string"}}}},"required":["account"],"additionalProperties":false}}}}]}}}}"#
                    ),
                    Some("tools/call") => {
                        seen_calls.store(true, Ordering::SeqCst);
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"deal created"}]}}"#
                            .to_string()
                    }
                    _ => "{}".to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), tool_calls)
    }

    /// Builds the same shape of router the orchestrator builds once per process.
    ///
    /// Deliberately left at the default [`CallOrigin::Production`]: the shared
    /// router serves production sessions and trial sessions alike, which is the
    /// exact condition that made the origin ceiling a no-op for trials.
    async fn shared_production_router(
        url: &str,
        sandbox_root: &std::path::Path,
    ) -> moa_hands::ToolRouter {
        let mut config = MoaConfig::default();
        config.local.sandbox_dir = sandbox_root.display().to_string();
        config.local.docker_enabled = false;
        config.security_profile = moa_config::SecurityProfile::Local;
        config.mcp_servers = vec![moa_config::McpServerConfig {
            required: true,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: TRIAL_CONNECTOR.to_string(),
            url: url.to_string(),
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: Vec::new(),
        }];
        let guard = Arc::new(moa_security::McpEgressGuard::new(Arc::new(
            moa_memory_pii::MockClassifier {
                fixed: moa_memory_pii::PiiResult {
                    class: moa_core::types::security::SensitivityClass::None,
                    spans: Vec::new(),
                    model_version: "experiment-call-origin-test".to_string(),
                    abstained: false,
                },
            },
        )));
        moa_hands::ToolRouter::from_config(&config, Some(guard), None)
            .await
            .expect("router with a discovered connector")
    }

    fn connector_invocation() -> moa_core::types::completion::ToolInvocation {
        moa_core::types::completion::ToolInvocation {
            id: None,
            name: moa_hands::mcp_tool_reference(TRIAL_CONNECTOR, TRIAL_CONNECTOR_TOOL),
            input: serde_json::json!({ "account": "acme" }),
        }
    }

    fn trial_operator_identity(tenant_id: TenantId) -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(0x0f01),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    fn call_origin_trial_record() -> ExperimentTrialRecord {
        ExperimentTrialRecord {
            scope: ActionRuleScope::Tenant {
                tenant_id: TenantId(Uuid::from_u128(0x0a01)),
            },
            trial_uid: Uuid::from_u128(0x0e02),
            run_uid: Uuid::from_u128(0x0e01),
            trial_key: "scenario/persona/profile/variant/0".to_string(),
            status: ExperimentTrialStatus::Running,
            target_kind: ExperimentTargetKind::AgentLoop,
            variant_key: "baseline".to_string(),
            plan_revision_uid: Uuid::from_u128(0x0e03),
            persona_id: None,
            profile_id: None,
            scenario_id: None,
            data_bundle_ids: Vec::new(),
            artifact_revision_uids: Vec::new(),
            simulator: moa_experiments::model::ExperimentSimulatorConfig {
                model: ModelId::new("sim"),
                temperature: None,
                max_turns: 1,
                token_budget: None,
                metadata: Value::Null,
            },
            target_model: None,
            seed: None,
            session_id: None,
            execution_run_uid: None,
            score_run_id: Uuid::from_u128(0x0e04),
            final_evidence_hash: None,
            turn_count: 0,
            stop_reason: None,
            error: None,
            trace_id: None,
            started_at: None,
            completed_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn a_caller_named_trial_session_is_admitted_only_with_this_trial_s_own_eval_origin_offline() {
        // Pins: the execution-template target lets a caller name any Session it can
        // reach, so tenant and contact scope alone would hand an eval trial a
        // production Session — and a production origin composes with the
        // process-wide production router into the full production capability set.
        // Only a Session carrying exactly this trial's eval-owned origin is
        // admitted: a sibling trial's Session, the run's own trial-less Session,
        // another run's Session, a sandbox origin, and an ordinary production
        // Session are all refused before the trial reads or writes through them.
        let trial = call_origin_trial_record();
        let tenant_id = trial.scope.tenant_id();
        let identity = trial_operator_identity(tenant_id);
        let expected = trial_call_origin(&trial);
        let owned = internal_execution_session_meta(
            SessionId::new(),
            trial.scope,
            ModelId::new("claude-sonnet-4-6"),
            Utc::now(),
            &identity,
            expected,
        )
        .expect("execution-template trial session metadata");

        assert_eq!(
            admit_caller_named_execution_session(&owned, tenant_id, trial.scope, expected)
                .expect("a Session carrying this trial's own origin is the supported target"),
            None
        );

        let production = SessionMeta {
            call_origin: CallOrigin::Production,
            ..owned.clone()
        };
        let refusal = handler_error_message(
            &admit_caller_named_execution_session(&production, tenant_id, trial.scope, expected)
                .expect_err("a production-origin Session must never host an experiment trial"),
        );
        assert!(
            refusal.contains("production") && refusal.contains("call origin"),
            "the refusal must name the origin that was rejected: {refusal}"
        );

        for (label, origin) in [
            (
                "a sibling trial of the same run",
                CallOrigin::Experiment {
                    run_uid: trial.run_uid,
                    trial_uid: Some(Uuid::from_u128(0x0e77)),
                },
            ),
            (
                "the run's own trial-less session",
                CallOrigin::Experiment {
                    run_uid: trial.run_uid,
                    trial_uid: None,
                },
            ),
            (
                "another eval run's session",
                CallOrigin::Experiment {
                    run_uid: Uuid::from_u128(0x0e78),
                    trial_uid: Some(trial.trial_uid),
                },
            ),
            ("a sandbox origin", CallOrigin::GeneratedCode),
        ] {
            let foreign = SessionMeta {
                call_origin: origin,
                ..owned.clone()
            };
            assert!(
                admit_caller_named_execution_session(&foreign, tenant_id, trial.scope, expected)
                    .is_err(),
                "{label} is not the unit being executed and must be refused"
            );
        }

        let foreign_tenant = SessionMeta {
            tenant_id: TenantId::new(),
            ..owned
        };
        assert!(
            admit_caller_named_execution_session(&foreign_tenant, tenant_id, trial.scope, expected)
                .is_err(),
            "a cross-tenant Session stays refused whatever origin it carries"
        );
    }

    #[tokio::test]
    async fn a_trial_session_of_either_target_kind_cannot_reach_a_production_connector_offline() {
        // Pins: both trial target kinds create their session through a constructor
        // that stamps the trial's eval-owned origin, and that stamp alone stops a
        // trial from holding a production connector on the shared, production-origin
        // router every governed tool call actually goes through. The refusal is
        // asserted on the policy path AND on the durable recovery path the tool
        // executor takes, the connector never receives a tools/call, and the exact
        // same invocation on the exact same router succeeds for a production-origin
        // session — so what changed is the origin, not a broken connector, a failed
        // discovery, or a rejected input.
        use std::sync::atomic::Ordering;

        let (connector_url, tool_calls) = spawn_recording_connector().await;
        let dir = tempfile::tempdir().expect("sandbox dir");
        let router = shared_production_router(&connector_url, dir.path()).await;
        assert!(
            router.call_origin().is_production(),
            "the shared orchestrator router is production-origin, so the session has to carry the trial ceiling"
        );

        let trial = call_origin_trial_record();
        let tenant_id = trial.scope.tenant_id();
        let identity = trial_operator_identity(tenant_id);
        let model = ModelId::new("claude-sonnet-4-6");
        let expected_origin = CallOrigin::Experiment {
            run_uid: trial.run_uid,
            trial_uid: Some(trial.trial_uid),
        };

        let agent_loop = new_session_meta(
            tenant_id,
            model.clone(),
            &identity,
            trial_call_origin(&trial),
        )
        .expect("agent-loop trial session metadata");
        let execution_template = internal_execution_session_meta(
            SessionId::new(),
            trial.scope,
            model.clone(),
            Utc::now(),
            &identity,
            trial_call_origin(&trial),
        )
        .expect("execution-template trial session metadata");

        for (target_kind, session) in [
            ("agent_loop", agent_loop),
            ("execution_template", execution_template),
        ] {
            assert_eq!(
                session.call_origin, expected_origin,
                "the {target_kind} trial session must name the owning run and trial"
            );

            let policy_error = router
                .check_policy(&session, &connector_invocation())
                .await
                .expect_err("a trial session must not hold a connector capability");
            assert!(
                matches!(&policy_error, moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains("production connectors")
                        && message.contains(TRIAL_CONNECTOR)),
                "the {target_kind} refusal must name the connector and the reason: {policy_error:?}"
            );

            let durable_error = router
                .execute_authorized_with_recovery(
                    &session,
                    &identity,
                    None,
                    &connector_invocation(),
                    moa_core::types::identifiers::ToolCallId::new(),
                    None,
                )
                .await
                .expect_err("the durable path must refuse the same capability");
            assert!(
                matches!(
                    durable_error,
                    moa_core::error::MoaError::PermissionDenied(_)
                ),
                "the {target_kind} durable path skips action policy, so it must carry its own admission: {durable_error:?}"
            );
        }

        assert!(
            !tool_calls.load(Ordering::SeqCst),
            "no refused trial path may reach the connector"
        );

        let production = SessionMeta {
            tenant_id,
            model,
            ..SessionMeta::default()
        };
        assert!(production.call_origin.is_production());
        let secured = router
            .execute_authorized_with_recovery(
                &production,
                &identity,
                None,
                &connector_invocation(),
                moa_core::types::identifiers::ToolCallId::new(),
                None,
            )
            .await
            .expect("production traffic keeps the same connector on the same router");
        assert_eq!(secured.safe_output.to_text(), "deal created");
        assert!(tool_calls.load(Ordering::SeqCst));
    }

    #[test]
    fn transcript_from_events_reconstructs_target_conversation_offline() {
        // Pins: a resumed simulator keeps prior target context from the durable session log.
        let session_id = SessionId::new();
        let events = vec![
            event_record(
                session_id,
                1,
                Event::UserMessage {
                    text: "first simulator turn".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                session_id,
                2,
                Event::BrainResponse {
                    text: "first target response".to_string(),
                    thought_signature: None,
                    model: ModelId::new("gpt-5.1"),
                    model_tier: ModelTier::Main,
                    input_tokens_uncached: 10,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 5,
                    cost_cents: 1,
                    duration_ms: 25,
                    llm_ttft_ms: None,
                },
            ),
        ];

        let transcript = transcript_from_events(&events);

        assert_eq!(latest_sequence(&events), 2);
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, MessageRole::User);
        assert_eq!(transcript[0].content, "first simulator turn");
        assert_eq!(transcript[1].role, MessageRole::Assistant);
        assert_eq!(
            transcript[1].content,
            "Target response: first target response"
        );
    }

    #[test]
    fn resumed_trial_usage_counts_only_events_after_its_durable_boundary_offline() {
        // Pins: resuming a trial includes its earlier target usage without
        // charging activity that predates the trial on a reused session.
        let session_id = SessionId::new();
        let response =
            |text: &str, input_tokens_uncached, output_tokens, cost_cents| Event::BrainResponse {
                text: text.to_string(),
                thought_signature: None,
                model: ModelId::new("gpt-5.1"),
                model_tier: ModelTier::Main,
                input_tokens_uncached,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens,
                cost_cents,
                duration_ms: 25,
                llm_ttft_ms: None,
            };
        let events = vec![
            event_record(session_id, 1, response("before trial", 100, 50, 9)),
            event_record(session_id, 2, response("trial turn one", 10, 5, 1)),
            event_record(session_id, 3, response("trial turn two", 20, 7, 2)),
        ];

        assert_eq!(target_usage_from_events_after(&events, 1), (42, 3));
        assert_eq!(target_usage_from_events_after(&events, 3), (0, 0));
    }

    #[test]
    fn agent_loop_trial_target_cannot_name_a_caller_session_offline() {
        // Pins: an agent-loop trial target payload has no way to name a session,
        // so a wire payload carrying one still resolves to an eval-owned run,
        // and simulator trials still refuse to inherit target attachments.
        let smuggled = serde_json::json!({
            "kind": "agent_loop",
            "prompt": "measure this behavior",
            "session_id": SessionId::new(),
            "model": "target-model",
            "attachments": [],
        });
        let target = serde_json::from_value::<ExperimentTarget>(smuggled)
            .expect("an agent-loop target payload parses without a session field");
        assert_eq!(target, agent_loop_target(Vec::new()));

        let attachment_error =
            agent_loop_target_selection(agent_loop_target(vec![test_attachment()]), None, None)
                .expect_err("simulator trials must not accept target attachments");
        assert!(
            format!("{attachment_error:?}").contains("attachments"),
            "unexpected rejection: {attachment_error:?}"
        );

        let selection = agent_loop_target_selection(
            agent_loop_target(Vec::new()),
            Some(ModelId::new("trial-model")),
            Some(ModelId::new("variant-model")),
        )
        .expect("an eval-owned agent-loop target resolves");
        assert_eq!(selection.model, Some(ModelId::new("trial-model")));
        let selection = agent_loop_target_selection(agent_loop_target(Vec::new()), None, None)
            .expect("an eval-owned agent-loop target resolves");
        assert_eq!(selection.model, Some(ModelId::new("target-model")));
    }

    #[test]
    fn resumed_trial_session_must_belong_to_the_trial_scope_offline() {
        // Pins: the ownership check a resumed trial runs before its first event
        // read rejects a session from another tenant or another contact.
        let tenant_id = TenantId::new();
        let scope = ActionRuleScope::Tenant { tenant_id };
        let owned = SessionMeta {
            tenant_id,
            ..SessionMeta::default()
        };
        require_trial_session_ownership(&owned, tenant_id, scope)
            .expect("the trial's own session passes ownership");

        let foreign = SessionMeta {
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        };
        let error = require_trial_session_ownership(&foreign, tenant_id, scope)
            .expect_err("a session owned by another tenant must be rejected");
        assert!(
            format!("{error:?}").contains("does not match the experiment trial scope"),
            "unexpected rejection: {error:?}"
        );

        let contact_id = ContactId::new();
        let contact_scope = ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        };
        let other_contact = SessionMeta {
            tenant_id,
            contact: Some(contact_ref(tenant_id, ContactId::new())),
            ..SessionMeta::default()
        };
        require_trial_session_ownership(&other_contact, tenant_id, contact_scope)
            .expect_err("a session owned by another contact must be rejected");
        let same_contact = SessionMeta {
            tenant_id,
            contact: Some(contact_ref(tenant_id, contact_id)),
            ..SessionMeta::default()
        };
        require_trial_session_ownership(&same_contact, tenant_id, contact_scope)
            .expect("the scoped contact session passes ownership");
    }

    fn agent_loop_target(
        attachments: Vec<moa_core::types::channel::Attachment>,
    ) -> ExperimentTarget {
        ExperimentTarget::AgentLoop {
            prompt: "measure this behavior".to_string(),
            agent: None,
            model: ModelId::new("target-model"),
            attachments,
        }
    }

    fn test_attachment() -> moa_core::types::channel::Attachment {
        moa_core::types::channel::Attachment {
            id: None,
            name: "receipt.png".to_string(),
            mime_type: Some("image/png".to_string()),
            sha256: None,
            url: None,
            path: None,
            size_bytes: None,
        }
    }

    fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Unverified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }

    #[test]
    fn execution_status_stops_trial_only_for_terminal_states_offline() {
        // Pins: typed Execution/status polling never finalizes an active or waiting run.
        for status in [
            ExecutionRunStatus::AwaitingConfirmation,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
            ExecutionRunStatus::WaitingInput,
            ExecutionRunStatus::WaitingReview,
            ExecutionRunStatus::WaitingReplan,
        ] {
            assert_eq!(trial_stop_for_execution_run_status(status), None);
        }
        assert_eq!(
            trial_stop_for_execution_run_status(ExecutionRunStatus::Completed),
            Some(WorkflowTrialStop {
                status: ExperimentTrialStatus::Completed,
                stop_reason: ExperimentTrialStopReason::TargetTerminal,
            })
        );
        assert_eq!(
            trial_stop_for_execution_run_status(ExecutionRunStatus::Cancelled),
            Some(WorkflowTrialStop {
                status: ExperimentTrialStatus::Cancelled,
                stop_reason: ExperimentTrialStopReason::Cancelled,
            })
        );
        for status in [
            ExecutionRunStatus::Partial,
            ExecutionRunStatus::Blocked,
            ExecutionRunStatus::Unsupported,
            ExecutionRunStatus::Failed,
        ] {
            assert_eq!(
                trial_stop_for_execution_run_status(status),
                Some(WorkflowTrialStop {
                    status: ExperimentTrialStatus::Failed,
                    stop_reason: ExperimentTrialStopReason::Error,
                })
            );
        }
    }

    #[test]
    fn experiment_execution_session_id_is_replay_stable_and_trial_specific_offline() {
        // Pins: target-session-null trials use the exact deterministic authority key and never
        // collide with the parent run target or another trial.
        let tenant_id = TenantId(Uuid::from_u128(1));
        let run_uid = Uuid::from_u128(2);
        let score_run_id = Uuid::from_u128(3);
        let trial_uid = Uuid::from_u128(4);
        let first =
            experiment_execution_session_id(tenant_id, run_uid, score_run_id, Some(trial_uid))
                .expect("deterministic trial Session id");
        assert_eq!(
            first,
            SessionId(
                Uuid::parse_str("84d778fa-591d-544f-89aa-b14f415ef956")
                    .expect("Task 9 golden Session id")
            )
        );
        assert_eq!(
            first,
            experiment_execution_session_id(tenant_id, run_uid, score_run_id, Some(trial_uid),)
                .expect("replayed deterministic trial Session id")
        );
        assert_ne!(
            first,
            experiment_execution_session_id(tenant_id, run_uid, score_run_id, None)
                .expect("run-target deterministic Session id")
        );
        assert_ne!(
            first,
            experiment_execution_session_id(
                tenant_id,
                run_uid,
                score_run_id,
                Some(Uuid::from_u128(5)),
            )
            .expect("second trial deterministic Session id")
        );
    }

    #[test]
    fn experiment_trial_operation_key_is_exact_offline() {
        // Pins: trial-owned compiler audit replay uses the Task 9 permanent operation key.
        assert_eq!(
            experiment_trial_operation_key(
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
            ),
            "experiment:00000000-0000-0000-0000-000000000001:\
             00000000-0000-0000-0000-000000000002:\
             00000000-0000-0000-0000-000000000003"
        );
    }

    #[test]
    fn trial_template_constructor_uses_exact_experiment_provenance_offline() {
        // Pins: the trial-target constructor writes explicit-run experiment identity and the
        // exact non-null trial UID without replacing effective Session authority.
        assert_eq!(
            experiment_template_source_provenance(
                "skill://durable-report".to_string(),
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
                Uuid::from_u128(4),
            ),
            ExecutionSourceProvenance::ExperimentTemplate {
                skill_template_ref: "skill://durable-report".to_string(),
                skill_template_revision_uid: Uuid::from_u128(1),
                experiment_run_uid: Uuid::from_u128(2),
                score_run_id: Uuid::from_u128(3),
                trial_uid: Some(Uuid::from_u128(4)),
            }
        );
    }

    #[test]
    fn target_turn_completion_signal_maps_to_session_status_offline() {
        // Pins: target waits consume a turn outcome signal instead of polling session status.
        let completed = TurnOutcome {
            turn_id: "turn-1".to_string(),
            kind: TurnOutcomeKind::Completed,
            message: "done".to_string(),
        };
        let failed = TurnOutcome {
            turn_id: "turn-2".to_string(),
            kind: TurnOutcomeKind::Failed,
            message: "failed".to_string(),
        };
        let raw = serde_json::to_string(&completed).expect("turn outcome serializes");

        assert_eq!(
            parse_turn_outcome(&raw).expect("turn outcome parses"),
            completed
        );
        assert_eq!(
            status_for_turn_outcome(&completed).expect("completed"),
            SessionStatus::Paused
        );
        assert_eq!(
            status_for_turn_outcome(&failed).expect("failed"),
            SessionStatus::Failed
        );
        let accepted = TurnOutcome {
            turn_id: "turn-3".to_string(),
            kind: TurnOutcomeKind::Accepted {
                execution_run_uid: Uuid::new_v4(),
            },
            message: "accepted".to_string(),
        };
        let error = status_for_turn_outcome(&accepted)
            .expect_err("legacy experiment AgentLoop cannot admit an execution run");
        assert!(format!("{error:?}").contains("run_requires_user_message_origin"));
    }

    fn event_record(session_id: SessionId, sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::new_v4(),
            session_id,
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }
}
