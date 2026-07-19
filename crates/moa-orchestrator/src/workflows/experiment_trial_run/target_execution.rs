//! Target execution paths for behavior-lab trial workflows.

use super::status::{
    attach_trial_execution_run, attach_trial_session, increment_trial_turn, stop_trial,
};
use super::trial_simulator::{SimulatorContext, simulator_done, simulator_next_user_message};
use super::*;
use crate::objects::session::{AttachSessionTurnWaiterInput, RemoveSessionTurnWaiterInput};
use crate::services::execution::ExecutionClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::{ctx::OrchestratorCtx, workflows::durable_utc_now};
use moa_artifacts::{
    canonical::canonical_json_bytes as artifact_canonical_json_bytes,
    execution_plan::{ExecutionGoalContract, GeneratedExecutionCandidate},
    reference::ArtifactRef,
};
use moa_core::types::{
    agent::AgentContext,
    contact::{ContactId, ContactRef, ContactVerificationState},
    execution_planning::{
        ExecutionAuditViolation, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload, ExecutionSourceProvenance,
        PinnedExecutionTemplateRef, bounded_audit_report, canonical_json_bytes,
        execution_planning_hash,
    },
};
use moa_core::wire::session_store::AppendEventRequest;
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
use std::{str::FromStr, time::Instant};

const EXECUTION_TARGET_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const EXECUTION_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const EXPERIMENT_EXECUTION_SESSION_NAMESPACE: Uuid =
    Uuid::from_u128(0xc2a6_731c_2d80_5d4a_9d10_2d20_1283_c6ec);
const EXPERIMENT_EXECUTION_SESSION_DOMAIN: &str = "moa.experiment.execution-session";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetObservation {
    status: SessionStatus,
    latest_response: Option<String>,
    latest_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetUsageObservation {
    latest_sequence: u64,
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
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target.clone())?;
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let (session_id, target_model) =
        ensure_agent_loop_session(ctx, &request, &trial, target, variant, pool, session_store)
            .await?;
    ctx.set(K_SESSION_ID, Json(session_id));
    tracing::Span::current().set_attribute("moa.experiment.session_id", session_id.to_string());

    let initial_events =
        load_session_events(ctx, session_id, EventRange::all(), session_store).await?;
    let mut transcript = transcript_from_events(&initial_events);
    let mut transcript_sequence = latest_sequence(&initial_events);
    let mut target_usage_sequence = transcript_sequence;
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
            return stop_trial(
                ctx,
                trial.scope,
                trial.trial_uid,
                stop.0,
                stop.1,
                None,
                pool,
            )
            .await;
        }
        if let Some(response) = observation.latest_response {
            transcript.push(ContextMessage::assistant(format!(
                "Target response: {response}"
            )));
        }
        transcript_sequence = observation.latest_sequence;

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
            return stop_trial(
                ctx,
                trial.scope,
                trial.trial_uid,
                ExperimentTrialStatus::Completed,
                ExperimentTrialStopReason::SimulatorDone,
                None,
                pool,
            )
            .await;
        }

        let response = with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .queue_message(Json::from(QueueMessageRequest {
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
        transcript.push(ContextMessage::user(simulator_message));

        let status =
            wait_for_target_after_turn(ctx, &request.identity, session_id, turn_id).await?;
        record_target_usage_after(ctx, session_id, &mut target_usage_sequence, session_store)
            .await?;
        if let Some(stop) = stop_for_session_status(&status) {
            return stop_trial(
                ctx,
                trial.scope,
                trial.trial_uid,
                stop.0,
                stop.1,
                None,
                pool,
            )
            .await;
        }
    }

    stop_trial(
        ctx,
        trial.scope,
        trial.trial_uid,
        ExperimentTrialStatus::Completed,
        ExperimentTrialStopReason::MaxTurns,
        None,
        pool,
    )
    .await
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
    let (target_session_id, target_agent, target_model, attachments_empty) = match target {
        ExperimentTarget::AgentLoop {
            session_id,
            agent,
            model,
            attachments,
            ..
        } => (
            session_id,
            agent,
            trial.target_model.clone().or(variant.model).or(Some(model)),
            attachments.is_empty(),
        ),
        ExperimentTarget::ExecutionTemplate { .. } => {
            return Err(bad_request(
                "agent-loop trial received a workflow experiment target",
            ));
        }
    };
    if !attachments_empty {
        return Err(bad_request(
            "simulator trials do not copy target prompt attachments into simulator turns",
        ));
    }

    let scope = trial.scope;
    let session_id = match trial.session_id.or(target_session_id) {
        Some(session_id) => session_id,
        None => {
            let model = target_model
                .clone()
                .ok_or_else(|| bad_request("agent-loop trial requires a target model"))?;
            let agent = target_agent.ok_or_else(|| {
                bad_request("agent-loop simulator target requires an agent selector")
            })?;
            let (session_id, meta) = create_new_session(
                ctx,
                request.tenant_id,
                model,
                &request.identity,
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
            ctx.sleep(SESSION_AUTHZ_PROPAGATION_DELAY).await?;
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
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
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
    stop_trial(
        ctx,
        trial.scope,
        trial.trial_uid,
        stop.status,
        stop.stop_reason,
        error,
        pool,
    )
    .await
}

async fn ensure_execution_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
    trial: &ExperimentTrialRecord,
    variant: &ExperimentVariant,
    target_session_id: Option<SessionId>,
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
        let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
        if meta.tenant_id != request.tenant_id || contact_id != trial.scope.contact_id() {
            return Err(TerminalError::new_with_code(
                409,
                "execution-template target Session does not match experiment scope",
            )
            .into());
        }
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
    let config = OrchestratorCtx::current_config();
    let model = trial
        .target_model
        .clone()
        .or_else(|| variant.model.clone())
        .unwrap_or_else(|| ModelId::new(config.models.main.clone()));
    let now = durable_utc_now(ctx, "experiment_trial_internal_session_now").await?;
    let meta =
        internal_execution_session_meta(session_id, trial.scope, model, now, &request.identity)?;
    let store = session_store.clone();
    let init_pool = pool.clone();
    let init_meta = meta.clone();
    let identity = request.identity.clone();
    let initialized = ctx
        .run(|| async move {
            crate::services::session_store::inner::initialize_internal_execution_session_atomic(
                store.as_ref(),
                &init_pool,
                init_meta,
                identity,
            )
            .await
            .map(Json::from)
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

fn internal_execution_session_meta(
    session_id: SessionId,
    scope: ActionRuleScope,
    model: ModelId,
    now: chrono::DateTime<Utc>,
    identity: &Identity,
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
    let config = OrchestratorCtx::current_config();
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

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
    agent: AgentSessionSelection,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = session_store.clone();
    let pool = pool.clone();
    let identity = identity.clone();
    Ok(ctx
        .run(|| async move {
            let mut meta = new_session_meta(tenant_id, model, &identity)?;
            let agent_context =
                resolve_agent_context_for_session(pool.clone(), &meta, &agent).await?;
            apply_agent_model_policy(&mut meta, &agent_context)?;
            meta.agent_context = Some(agent_context);
            let session_id =
                create_session_for_identity(store.as_ref(), &pool, meta.clone(), identity)
                    .await
                    .map_err(non_retryable_handler_error)?;
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
) -> Result<(), HandlerError> {
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
            }))
        })
        .name("experiment_trial_record_target_usage")
        .await?
        .into_inner();
    *sequence_num = observation.latest_sequence;
    Ok(())
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
