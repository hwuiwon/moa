//! Target execution paths for behavior-lab experiment runs.

use super::*;

use std::{str::FromStr, time::Instant};

use moa_artifacts::{
    canonical::canonical_json_bytes as artifact_canonical_json_bytes,
    execution_plan::{ExecutionGoalContract, GeneratedExecutionCandidate},
    reference::ArtifactRef,
};
use moa_core::{
    events::Event,
    types::{
        agent::AgentContext,
        contact::{ContactId, ContactRef, ContactVerificationState},
        events_stream::EventRecord,
        execution_planning::{
            ExecutionAuditViolation, ExecutionCompileOutcome, ExecutionCompileSource,
            ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
            ExecutionSourceProvenance, PinnedExecutionTemplateRef, bounded_audit_report,
            canonical_json_bytes, execution_planning_hash,
        },
    },
    wire::session_store::AppendEventRequest,
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

use crate::{
    ctx::OrchestratorCtx,
    services::{execution::ExecutionClient, session_store::RestateSessionStoreClient},
};

const EXECUTION_TARGET_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
const EXECUTION_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const EXPERIMENT_EXECUTION_SESSION_NAMESPACE: Uuid =
    Uuid::from_u128(0xc2a6_731c_2d80_5d4a_9d10_2d20_1283_c6ec);
const EXPERIMENT_EXECUTION_SESSION_DOMAIN: &str = "moa.experiment.execution-session";

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

#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
pub(super) async fn run_agent_loop_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    prompt: String,
    session_id: Option<SessionId>,
    agent: Option<AgentSessionSelection>,
    model: ModelId,
    attachments: Vec<moa_core::types::channel::Attachment>,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let scope = tenant_scope(request.tenant_id);
    persist_run_status(
        ctx,
        request.tenant_id,
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
        pool,
    )
    .await?;

    let model = variant.model.unwrap_or(model);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            let agent = agent.ok_or_else(|| {
                bad_request("agent-loop experiment target requires an agent selector")
            })?;
            let (session_id, meta) = create_new_session(
                ctx,
                request.tenant_id,
                model.clone(),
                &request,
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
            // The session authz tuples are applied through the normal outbox poller.
            ctx.sleep(Duration::from_millis(750)).await?;
            session_id
        }
    };

    ctx.set(K_SESSION_ID, Json(session_id));
    tracing::Span::current().set_attribute("moa.experiment.session_id", session_id.to_string());
    persist_attached_session(ctx, scope, request.run_uid, session_id, pool).await?;

    with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .queue_message(Json::from(QueueMessageRequest {
                user_message: prompt,
                attachments,
                model: Some(model.to_string()),
                contact: None,
                max_turns: None,
                execution_template: None,
            })),
        &request.identity,
    )
    .call()
    .await?;

    run_status_response(
        ctx,
        ExperimentRunStatusRequest {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
        pool,
        session_store,
    )
    .await
}

/// Executes one pinned execution-template target through typed Execution services.
#[allow(
    clippy::too_many_arguments,
    reason = "the workflow target keeps durable input and concrete stores explicit instead of hiding them in a dependency bag"
)]
pub(super) async fn run_execution_template_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    template: PinnedExecutionTemplateRef,
    objective: String,
    input: Value,
    target_session_id: Option<SessionId>,
    idempotency_key: Option<String>,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    if objective.trim().is_empty() {
        return Err(bad_request(
            "execution-template experiment objective must not be empty",
        ));
    }
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    if variant.execution_template.as_ref() != Some(&template) {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template target and variant do not pin the same revision",
        )
        .into());
    }
    let scope = tenant_scope(request.tenant_id);
    persist_run_status(
        ctx,
        request.tenant_id,
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
        pool,
    )
    .await?;

    let effective = ensure_execution_session(
        ctx,
        &request,
        scope,
        variant.model,
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
    persist_attached_session(ctx, scope, request.run_uid, effective.session_id, pool).await?;

    let origin = append_experiment_objective(
        ctx,
        effective.session_id,
        &objective,
        request.run_uid,
        request.score_run_id,
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
    let now = durable_utc_now(ctx, "experiment_execution_compile_now").await?;
    let compiled = compile_experiment_template(ExperimentTemplateCompileRequest {
        context: &planning_context.snapshot,
        requested: &template,
        objective,
        input,
        experiment_run_uid: request.run_uid,
        score_run_id: request.score_run_id,
        operation_key: experiment_run_operation_key(request.run_uid, request.score_run_id),
        now,
    })?;
    persist_compile_audit(ctx, scope, compiled.audit, pool).await?;
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
                        "experiment-run:{}:{}",
                        request.run_uid, request.score_run_id
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
    ctx.run(|| async move {
        attach_execution_run(attach_pool, scope, request.run_uid, execution_run_uid)
            .await
            .map(Json::from)
    })
    .name("experiment_attach_execution_run")
    .await?;
    ctx.set(K_EXECUTION_RUN_UID, Json(execution_run_uid));
    tracing::Span::current().set_attribute(
        "moa.experiment.execution_run_uid",
        execution_run_uid.to_string(),
    );

    let (status, error) = wait_for_execution_outcome(
        ctx,
        &request.identity,
        request.tenant_id,
        effective.contact_id,
        effective.session_id,
        execution_run_uid,
    )
    .await?;
    finalize_run_status(ctx, request.tenant_id, request.run_uid, status, error, pool).await?;

    run_status_response(
        ctx,
        ExperimentRunStatusRequest {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
        pool,
        session_store,
    )
    .await
}

async fn ensure_execution_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentRunWorkflowRequest,
    scope: ActionRuleScope,
    variant_model: Option<ModelId>,
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
            .name("experiment_load_execution_session")
            .await?
            .into_inner();
        let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
        if meta.tenant_id != scope.tenant_id() || contact_id != scope.contact_id() {
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

    let session_id =
        experiment_execution_session_id(request.tenant_id, request.run_uid, request.score_run_id)?;
    let config = OrchestratorCtx::current_config();
    let model = variant_model.unwrap_or_else(|| ModelId::new(config.models.main.clone()));
    let now = durable_utc_now(ctx, "experiment_internal_execution_session_now").await?;
    let meta = internal_execution_session_meta(session_id, scope, model, now, &request.identity)?;
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
        .name("experiment_initialize_internal_execution_session")
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
        contact_id: scope.contact_id(),
        target_session_supplied: false,
    })
}

async fn finalize_run_status(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let completed_at = durable_utc_now(ctx, "experiment_utc_now").await?;
    persist_run_status(
        ctx,
        tenant_id,
        run_uid,
        status,
        error,
        Some(completed_at),
        pool,
    )
    .await
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    request: &ExperimentRunWorkflowRequest,
    agent: AgentSessionSelection,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = session_store.clone();
    let pool = pool.clone();
    let identity = request.identity.clone();
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
        .name("experiment_create_session")
        .await?
        .into_inner())
}

fn new_session_meta(
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
) -> Result<SessionMeta, HandlerError> {
    let now = Utc::now();
    Ok(SessionMeta {
        id: SessionId::new(),
        tenant_id,
        title: Some("Experiment agent-loop run".to_string()),
        status: SessionStatus::Created,
        channel: Channel::Chat,
        model,
        created_at: now,
        updated_at: now,
        created_by: Some(session_actor_ref(identity)?),
        ..SessionMeta::default()
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
        title: Some("Experiment execution-template run".to_string()),
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

fn session_actor_ref(identity: &Identity) -> Result<SessionActorRef, HandlerError> {
    match identity.identity_type {
        IdentityType::Operator | IdentityType::Agent => Ok(SessionActorRef::Identity {
            id: identity.acting_on_behalf_of.unwrap_or(identity.id),
        }),
        IdentityType::Service => Err(TerminalError::new_with_code(
            403,
            "service identities cannot create experiment sessions",
        )
        .into()),
        IdentityType::Contact => Err(TerminalError::new_with_code(
            403,
            "contact identities cannot create experiment sessions",
        )
        .into()),
    }
}

fn experiment_execution_session_id(
    tenant_id: TenantId,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
) -> Result<SessionId, HandlerError> {
    let mut name = EXPERIMENT_EXECUTION_SESSION_DOMAIN.as_bytes().to_vec();
    append_nullable_frame(&mut name, Some(tenant_id.to_string().as_bytes()))?;
    append_nullable_frame(&mut name, Some(experiment_run_uid.to_string().as_bytes()))?;
    append_nullable_frame(&mut name, Some(score_run_id.to_string().as_bytes()))?;
    append_nullable_frame(&mut name, None)?;
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
                dedupe_key: Some(experiment_objective_dedupe_key(
                    experiment_run_uid,
                    score_run_id,
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

fn experiment_objective_dedupe_key(experiment_run_uid: Uuid, score_run_id: Uuid) -> String {
    format!("experiment-objective:{experiment_run_uid}:{score_run_id}:none")
}

fn experiment_run_operation_key(experiment_run_uid: Uuid, score_run_id: Uuid) -> String {
    format!("experiment:{experiment_run_uid}:{score_run_id}:none")
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
        ),
    })
}

fn experiment_template_source_provenance(
    skill_template_ref: String,
    skill_template_revision_uid: Uuid,
    experiment_run_uid: Uuid,
    score_run_id: Uuid,
) -> ExecutionSourceProvenance {
    ExecutionSourceProvenance::ExperimentTemplate {
        skill_template_ref,
        skill_template_revision_uid,
        experiment_run_uid,
        score_run_id,
        trial_uid: None,
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
                        "experiment compile audit persistence failed: {error}"
                    ))
                    .into()
                })
        })
        .name("experiment_write_compile_audit")
        .await?
        .into_inner();
    moa_brain::execution_planning::request::record_applied_planning_audit(&outcome);
    if matches!(outcome, CompileAuditWriteOutcome::Conflict { .. }) {
        return Err(TerminalError::new_with_code(
            409,
            "experiment run compile audit conflicts with first persisted evidence",
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
) -> Result<(ExperimentRunStatus, Option<String>), HandlerError> {
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
        if let Some(terminal) = experiment_status_for_execution_status(&status) {
            return Ok(terminal);
        }
        ctx.sleep(EXECUTION_STATUS_POLL_INTERVAL).await?;
    }
    let reason = format!(
        "experiment timed out waiting {EXECUTION_TARGET_WAIT_TIMEOUT:?} for execution run {run_uid}"
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
    Ok((ExperimentRunStatus::Failed, Some(reason)))
}

fn experiment_status_for_execution_status(
    response: &ExecutionStatusResponse,
) -> Option<(ExperimentRunStatus, Option<String>)> {
    let status = experiment_run_status_for_execution_run_status(response.run.status)?;
    let error = (status == ExperimentRunStatus::Failed).then(|| {
        format!(
            "execution run {} ended with status {} and gaps {:?}",
            response.run.run_uid,
            response.run.status.as_str(),
            response.gaps
        )
    });
    Some((status, error))
}

fn experiment_run_status_for_execution_run_status(
    status: ExecutionRunStatus,
) -> Option<ExperimentRunStatus> {
    Some(match status {
        ExecutionRunStatus::Completed => ExperimentRunStatus::Completed,
        ExecutionRunStatus::Cancelled => ExperimentRunStatus::Cancelled,
        ExecutionRunStatus::Partial
        | ExecutionRunStatus::Blocked
        | ExecutionRunStatus::Unsupported
        | ExecutionRunStatus::Failed => ExperimentRunStatus::Failed,
        ExecutionRunStatus::AwaitingConfirmation
        | ExecutionRunStatus::Queued
        | ExecutionRunStatus::Running
        | ExecutionRunStatus::WaitingInput
        | ExecutionRunStatus::WaitingReview
        | ExecutionRunStatus::WaitingReplan => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_execution_session_and_run_keys_are_replay_stable_offline() {
        // Pins: a sessionless run always reuses its exact UUIDv5 Session and the
        // run-owned objective/audit keys retain the literal null-trial suffix.
        let tenant_id = TenantId::new();
        let run_uid = Uuid::now_v7();
        let score_run_id = Uuid::now_v7();
        let first = experiment_execution_session_id(tenant_id, run_uid, score_run_id)
            .expect("valid UUID fields should derive an internal Session key");
        let replay = experiment_execution_session_id(tenant_id, run_uid, score_run_id)
            .expect("replay should derive the same internal Session key");

        assert_eq!(first, replay);
        assert_eq!(
            experiment_objective_dedupe_key(run_uid, score_run_id),
            format!("experiment-objective:{run_uid}:{score_run_id}:none")
        );
        assert_eq!(
            experiment_run_operation_key(run_uid, score_run_id),
            format!("experiment:{run_uid}:{score_run_id}:none")
        );
    }

    #[test]
    fn run_template_constructor_uses_exact_experiment_provenance_offline() {
        // Pins: the run-target constructor writes explicit-run experiment identity with a null
        // trial UID; effective Session authority remains in the planning/start envelopes.
        assert_eq!(
            experiment_template_source_provenance(
                "skill://durable-report".to_string(),
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
            ),
            ExecutionSourceProvenance::ExperimentTemplate {
                skill_template_ref: "skill://durable-report".to_string(),
                skill_template_revision_uid: Uuid::from_u128(1),
                experiment_run_uid: Uuid::from_u128(2),
                score_run_id: Uuid::from_u128(3),
                trial_uid: None,
            }
        );
    }

    #[test]
    fn execution_status_finalizes_experiment_run_only_for_terminal_states_offline() {
        // Pins: typed Execution/status polling leaves every active/waiting state
        // in flight and maps each terminal state to the experiment vocabulary.
        for status in [
            ExecutionRunStatus::AwaitingConfirmation,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
            ExecutionRunStatus::WaitingInput,
            ExecutionRunStatus::WaitingReview,
            ExecutionRunStatus::WaitingReplan,
        ] {
            assert_eq!(experiment_run_status_for_execution_run_status(status), None);
        }
        assert_eq!(
            experiment_run_status_for_execution_run_status(ExecutionRunStatus::Completed),
            Some(ExperimentRunStatus::Completed)
        );
        assert_eq!(
            experiment_run_status_for_execution_run_status(ExecutionRunStatus::Cancelled),
            Some(ExperimentRunStatus::Cancelled)
        );
        for status in [
            ExecutionRunStatus::Partial,
            ExecutionRunStatus::Blocked,
            ExecutionRunStatus::Unsupported,
            ExecutionRunStatus::Failed,
        ] {
            assert_eq!(
                experiment_run_status_for_execution_run_status(status),
                Some(ExperimentRunStatus::Failed)
            );
        }
    }
}
