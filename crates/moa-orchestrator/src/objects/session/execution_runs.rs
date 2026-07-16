//! Session-owned durable handoff for newly admitted execution runs.

use std::sync::Arc;

use moa_brain::execution_planning::request::record_applied_planning_audit;
use moa_brain::execution_planning::routing::record_applied_route_audit;
use moa_brain::execution_planning::{
    ExecutionPlanningRequest, ExecutionPlanningResultKind, ExecutionRoutingInput, plan_execution,
    route_execution,
};
use moa_core::traits::{LLMProvider, SessionStore};
use moa_core::types::completion::{CompletionRequest, CompletionStream};
use moa_core::types::execution_planning::{
    ExecutionMode, ExecutionPlanningAuditEnvelopeV1, ExecutionPlanningAuditPayloadV1,
    ExecutionRouteDecision, ExecutionRouteDecisionKind, ExecutionRouteReason, ExecutionRouteStage,
    execution_planning_dedupe_key, validate_planning_audit_envelope,
};
use moa_core::types::identifiers::ModelId;
use moa_core::types::model::ModelCapabilities;
use moa_execution::repository::{
    CompileAuditWriteOutcome, ExecutionRepository, ExecutionScope, PlannerCallAuditWriteOutcome,
    RouteAuditWriteOutcome,
};
use moa_session::PostgresSessionStore;

use super::*;
use crate::ctx::OrchestratorCtx;
use crate::services::execution::{
    record_execution_template_admission_origin, record_execution_template_admission_run,
    reserve_execution_template_admission,
};
use crate::workflows::execution_run::ExecutionRunClient;

const EXECUTION_SYNTHESIS_TURN_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0xf61c_9bb0_e9a7_5793_80f5_6a38_5d6e_8eb2);
const EXECUTION_SYNTHESIS_TURN_DOMAIN: &str = "moa.execution.synthesis-turn.v1";

struct TemplateAdmissionPlanner;

#[async_trait::async_trait]
impl LLMProvider for TemplateAdmissionPlanner {
    fn name(&self) -> &'static str {
        "external-template-admission"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        Err(MoaError::ProviderError(
            "external pinned-template admission cannot invoke a planning provider".to_string(),
        ))
    }
}

/// Admits one external pinned execution template through the owning Session.
pub(super) async fn admit_execution_template(
    ctx: &mut ObjectContext<'_>,
    state: &mut Tracked<SessionVoState>,
    session_store: Arc<PostgresSessionStore>,
    identity: &moa_core::traits::Identity,
    request: moa_execution::wire::ExecutionTemplateAdmissionRequest,
) -> Result<moa_execution::wire::ExecutionTemplateAdmissionResponse, HandlerError> {
    request
        .validate()
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    let session_id = parse_session_key(ctx.key())?;
    if request.session_id != session_id {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template admission request targets a different Session",
        )
        .into());
    }
    let meta = state
        .ensure_initialized()
        .map_err(crate::workflows::errors::moa_error_to_status_handler_error)?
        .clone();
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    if request.tenant_id != meta.tenant_id || request.contact_id != contact_id {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template admission scope does not match the owning Session",
        )
        .into());
    }

    let operation_uid = match request.idempotency_key.as_deref() {
        Some(key) => {
            moa_execution::wire::execution_template_admission_operation_uid(request.tenant_id, key)
                .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?
        }
        None => ctx.rand_uuid(),
    };
    let request_fingerprint =
        moa_execution::wire::execution_template_admission_request_fingerprint(&request)
            .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?
            .to_string();
    let pool = OrchestratorCtx::current_graph_pool();
    let reserved_request = request.clone();
    let reserved_fingerprint = request_fingerprint.clone();
    let replay = ctx
        .run(|| async move {
            let record = reserve_execution_template_admission(
                &pool,
                &reserved_request,
                operation_uid,
                &reserved_fingerprint,
            )
            .await
            .map_err(admission_persistence_error)?;
            Ok::<_, HandlerError>(Json::from(replay_state(record)))
        })
        .name(format!(
            "execution_template_admission_reserve_{operation_uid}"
        ))
        .await?
        .into_inner();

    let replay = match admission_resume(&replay, &request_fingerprint, session_id)? {
        ExecutionTemplateAdmissionResume::Complete(response) => return Ok(response),
        ExecutionTemplateAdmissionResume::AppendObjective => {
            let persisted =
                append_admission_objective(ctx, session_id, &request.objective, operation_uid)
                    .await?;
            let pool = OrchestratorCtx::current_graph_pool();
            let fingerprint = request_fingerprint.clone();
            let sequence = persisted.sequence_num;
            ctx.run(|| async move {
                let record = record_execution_template_admission_origin(
                    &pool,
                    request.tenant_id,
                    operation_uid,
                    &fingerprint,
                    sequence,
                )
                .await
                .map_err(admission_persistence_error)?;
                Ok::<_, HandlerError>(Json::from(replay_state(record)))
            })
            .name(format!(
                "execution_template_admission_origin_{operation_uid}"
            ))
            .await?
            .into_inner()
        }
        ExecutionTemplateAdmissionResume::StartExecution { .. } => replay,
    };

    let originating_user_sequence_num =
        match admission_resume(&replay, &request_fingerprint, session_id)? {
            ExecutionTemplateAdmissionResume::StartExecution {
                originating_user_sequence_num,
            } => originating_user_sequence_num,
            ExecutionTemplateAdmissionResume::Complete(response) => return Ok(response),
            ExecutionTemplateAdmissionResume::AppendObjective => {
                return Err(TerminalError::new_with_code(
                    500,
                    "execution-template admission objective sequence was not committed",
                )
                .into());
            }
        };

    let execution_run_uid = start_external_template_execution(
        ctx,
        session_store,
        identity,
        &request,
        operation_uid,
        originating_user_sequence_num,
    )
    .await?;
    state.apply_accepted_execution_turn(durable_utc_now(ctx).await?);
    state.persist(ctx);
    sync_status(ctx, session_id, state).await?;
    let pool = OrchestratorCtx::current_graph_pool();
    let fingerprint = request_fingerprint.clone();
    let completed = ctx
        .run(|| async move {
            let record = record_execution_template_admission_run(
                &pool,
                request.tenant_id,
                operation_uid,
                &fingerprint,
                execution_run_uid,
            )
            .await
            .map_err(admission_persistence_error)?;
            Ok::<_, HandlerError>(Json::from(replay_state(record)))
        })
        .name(format!("execution_template_admission_run_{operation_uid}"))
        .await?
        .into_inner();
    match admission_resume(&completed, &request_fingerprint, session_id)? {
        ExecutionTemplateAdmissionResume::Complete(response) => Ok(response),
        _ => Err(TerminalError::new_with_code(
            500,
            "execution-template admission did not commit its complete response",
        )
        .into()),
    }
}

fn replay_state(
    record: crate::services::execution::ExecutionTemplateAdmissionRecord,
) -> ExecutionTemplateAdmissionReplayState {
    ExecutionTemplateAdmissionReplayState {
        operation_uid: record.operation_uid,
        request_fingerprint: record.request_fingerprint,
        originating_user_sequence_num: record.originating_user_sequence_num,
        execution_run_uid: record.execution_run_uid,
    }
}

fn admission_resume(
    replay: &ExecutionTemplateAdmissionReplayState,
    request_fingerprint: &str,
    session_id: SessionId,
) -> Result<ExecutionTemplateAdmissionResume, HandlerError> {
    replay
        .resume(request_fingerprint, session_id)
        .map_err(|error| match error {
            MoaError::ValidationError(_) => {
                TerminalError::new_with_code(409, error.to_string()).into()
            }
            other => crate::workflows::errors::moa_error_to_handler_error(other),
        })
}

fn admission_persistence_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::ValidationError(_) => TerminalError::new_with_code(409, error.to_string()).into(),
        other => crate::workflows::errors::moa_error_to_handler_error(other),
    }
}

async fn append_admission_objective(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    objective: &str,
    operation_uid: uuid::Uuid,
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
                    "execution-template-admission-objective:{operation_uid}"
                )),
            })),
    )
    .call()
    .await?
    .into_inner();
    if persisted.event != event {
        return Err(TerminalError::new_with_code(
            409,
            "execution-template admission objective replay conflicts with the first event",
        )
        .into());
    }
    Ok(persisted)
}

#[allow(clippy::too_many_arguments)]
async fn start_external_template_execution(
    ctx: &ObjectContext<'_>,
    session_store: Arc<PostgresSessionStore>,
    identity: &moa_core::traits::Identity,
    request: &moa_execution::wire::ExecutionTemplateAdmissionRequest,
    operation_uid: uuid::Uuid,
    originating_user_sequence_num: u64,
) -> Result<uuid::Uuid, HandlerError> {
    let session_id = request.session_id;
    let invocation = moa_core::types::execution_planning::ExecutionTemplateInvocation {
        template: request.template.clone(),
        input: request.input.clone(),
    };
    let route = route_execution(ExecutionRoutingInput {
        objective: &request.objective,
        execution_template: Some(&invocation),
        escalation: None,
    });
    let ExecutionRouteDecision::Routed {
        mode: ExecutionMode::Run,
        reason: ExecutionRouteReason::SelectedExecutionTemplate,
    } = route
    else {
        return Err(TerminalError::new_with_code(
            422,
            "external execution-template admission did not select the Task 7 template route",
        )
        .into());
    };
    let accepted_at = durable_utc_now(ctx).await?;
    persist_execution_planning_audit(
        ctx,
        session_store.clone(),
        ExecutionPlanningAuditEnvelopeV1 {
            schema_version: 1,
            tenant_id: request.tenant_id,
            contact_id: request.contact_id,
            session_id: Some(session_id),
            originating_sequence: Some(originating_user_sequence_num),
            payload: ExecutionPlanningAuditPayloadV1::Route {
                stage: ExecutionRouteStage::Initial,
                decision: ExecutionRouteDecisionKind::Routed,
                mode: Some(ExecutionMode::Run),
                reason: ExecutionRouteReason::SelectedExecutionTemplate,
                accepted_at,
            },
        },
    )
    .await?;

    let planning_call = ctx
        .service_client::<ExecutionClient>()
        .planning_context(Json::from(
            moa_execution::wire::ExecutionPlanningContextRequest {
                tenant_id: request.tenant_id,
                contact_id: request.contact_id,
                session_id,
                originating_user_sequence_num,
                requested_template: Some(request.template.clone()),
            },
        ));
    let planning_context = with_identity_headers(planning_call, identity)
        .call()
        .await?
        .into_inner();

    let config = OrchestratorCtx::current_config();
    let planner_model = config
        .models
        .auxiliary
        .clone()
        .unwrap_or_else(|| config.models.main.clone());
    let planning_now = durable_utc_now(ctx).await?;
    let planned = plan_execution(
        &TemplateAdmissionPlanner,
        ExecutionPlanningRequest {
            objective: request.objective.clone(),
            context: planning_context.snapshot.clone(),
            execution_template: Some(invocation),
            escalation: None,
            planner_model: ModelId::new(planner_model),
            config: config.execution.clone(),
            now: planning_now,
        },
        ExecutionRouteReason::SelectedExecutionTemplate,
    )
    .await
    .map_err(crate::workflows::errors::moa_error_to_handler_error)?;
    for audit in planned.audits {
        persist_execution_planning_audit(ctx, session_store.clone(), audit).await?;
    }
    let admitted = match planned.kind {
        ExecutionPlanningResultKind::Ready(admitted) => admitted,
        ExecutionPlanningResultKind::NeedsInput { message }
        | ExecutionPlanningResultKind::Unsupported { message } => {
            return Err(TerminalError::new_with_code(422, message).into());
        }
    };

    let start_call = ctx.service_client::<ExecutionClient>().start(Json::from(
        moa_execution::wire::ExecutionStartRequest {
            tenant_id: request.tenant_id,
            contact_id: request.contact_id,
            session_id,
            originating_user_sequence_num,
            planning_context_uid: planning_context.planning_context_uid,
            planning_context_hash: planning_context.planning_context_hash,
            idempotency_key: Some(format!("external-admission:{operation_uid}")),
            compiled: admitted.compiled,
            run_input: admitted.run_input,
            source_provenance: admitted.source_provenance,
        },
    ));
    let started = with_identity_headers(start_call, identity)
        .call()
        .await?
        .into_inner();
    if started.run.originating_user_sequence_num != originating_user_sequence_num {
        return Err(TerminalError::new_with_code(
            409,
            "execution start replay returned a different originating user sequence",
        )
        .into());
    }
    Ok(started.run.run_uid)
}

/// Durably launches one committed run after the owning Session has activated it.
pub(super) fn dispatch_execution_run(
    ctx: &ObjectContext<'_>,
    state: &SessionVoState,
    run_uid: uuid::Uuid,
) -> Result<(), HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let meta = state
        .ensure_initialized()
        .map_err(crate::workflows::errors::moa_error_to_status_handler_error)?;
    crate::restate_identity::replay_safe_request(
        ctx.workflow_client::<ExecutionRunClient>(run_uid.to_string())
            .run(Json::from(
                moa_execution::wire::ExecutionRunWorkflowRequest {
                    run_uid,
                    tenant_id: meta.tenant_id,
                    contact_id: meta.contact.as_ref().map(|contact| contact.contact_id),
                    session_id,
                },
            )),
    )
    .send();
    Ok(())
}

/// Persists one normalized planning audit after validating its Session origin.
pub(super) async fn persist_execution_planning_audit(
    ctx: &ObjectContext<'_>,
    session_store: Arc<PostgresSessionStore>,
    envelope: ExecutionPlanningAuditEnvelopeV1,
) -> Result<(), HandlerError> {
    validate_planning_audit_envelope(&envelope)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    let state = Tracked::<SessionVoState>::load(ctx).await?;
    let meta = state
        .ensure_initialized()
        .map_err(crate::workflows::errors::moa_error_to_status_handler_error)?;
    let session_id = parse_session_key(ctx.key())?;
    let expected_contact = meta.contact.as_ref().map(|contact| contact.contact_id);
    if envelope.tenant_id != meta.tenant_id
        || envelope.contact_id != expected_contact
        || envelope.session_id != Some(session_id)
    {
        return Err(TerminalError::new_with_code(
            409,
            "execution planning audit scope does not match the owning session",
        )
        .into());
    }
    let originating_sequence = envelope.originating_sequence.ok_or_else(|| {
        TerminalError::new_with_code(409, "execution planning audit has no user origin")
    })?;
    let dedupe_key = execution_planning_dedupe_key(&envelope)
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    let durable_step_suffix = dedupe_key
        .strip_prefix("execution-planning-v1:")
        .unwrap_or(&dedupe_key)
        .to_string();
    let store = session_store.clone();
    let origin = ctx
        .run(|| async move {
            store
                .get_events(
                    session_id,
                    EventRange {
                        from_seq: Some(originating_sequence),
                        to_seq: Some(originating_sequence),
                        event_types: None,
                        limit: Some(1),
                    },
                )
                .await
                .map(Json::from)
                .map_err(crate::workflows::errors::moa_error_to_handler_error)
        })
        .name(format!(
            "execution_planning_audit_origin_{durable_step_suffix}"
        ))
        .await?
        .into_inner();
    if !matches!(
        origin.as_slice(),
        [EventRecord {
            event: Event::UserMessage { .. },
            ..
        }]
    ) {
        return Err(TerminalError::new_with_code(
            409,
            "execution planning audit origin is not an exact persisted user message",
        )
        .into());
    }
    let scope = envelope.contact_id.map_or(
        ExecutionScope::Tenant {
            tenant_id: envelope.tenant_id,
        },
        |contact_id| ExecutionScope::Contact {
            tenant_id: envelope.tenant_id,
            contact_id,
        },
    );
    match &envelope.payload {
        ExecutionPlanningAuditPayloadV1::Route { .. } => {
            let pool = session_store.pool().clone();
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
            record_applied_route_audit(&result);
            if matches!(result, RouteAuditWriteOutcome::Conflict { .. }) {
                return Err(planning_audit_conflict());
            }
        }
        ExecutionPlanningAuditPayloadV1::PlannerCall { .. } => {
            let pool = session_store.pool().clone();
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
            record_applied_planning_audit(&result);
            if matches!(result, PlannerCallAuditWriteOutcome::Conflict { .. }) {
                return Err(planning_audit_conflict());
            }
        }
        ExecutionPlanningAuditPayloadV1::Compile { .. } => {
            let pool = session_store.pool().clone();
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
            record_applied_planning_audit(&result);
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

/// Validates and publishes one terminal run plus its stable synthesis request.
pub(super) async fn accept_execution_terminal(
    ctx: &ObjectContext<'_>,
    state: &SessionVoState,
    delivery: moa_execution::wire::ExecutionTerminalDelivery,
) -> Result<ExecutionSynthesisRequested, HandlerError> {
    let run_uid = delivery.summary.run_uid;
    let origin = delivery.summary.originating_user_sequence_num;
    if let Some(existing) = state.execution_synthesis_marker(run_uid, origin) {
        return Ok(ExecutionSynthesisRequested {
            run_uid,
            originating_user_sequence_num: origin,
            turn_id: existing.turn_id.clone(),
            terminal: delivery.summary,
            run_evidence: ExecutionRunEvidenceRef::ExecutionRun { run_uid },
        });
    }
    let Some(active) = state
        .active_execution_runs
        .iter()
        .find(|active| active.run_uid == run_uid)
    else {
        return Err(TerminalError::new_with_code(
            409,
            "execution terminal delivery references an inactive run",
        )
        .into());
    };
    if active.originating_user_sequence_num != origin {
        return Err(TerminalError::new_with_code(
            409,
            "execution terminal origin conflicts with admitted run",
        )
        .into());
    }

    let terminal_event = match delivery.status {
        moa_execution::state::ExecutionRunStatus::Completed => {
            Event::ExecutionCompleted(delivery.summary.clone())
        }
        moa_execution::state::ExecutionRunStatus::Cancelled => {
            Event::ExecutionCancelled(delivery.summary.clone())
        }
        status @ (moa_execution::state::ExecutionRunStatus::Partial
        | moa_execution::state::ExecutionRunStatus::Blocked
        | moa_execution::state::ExecutionRunStatus::Unsupported
        | moa_execution::state::ExecutionRunStatus::Failed) => Event::ExecutionFailed {
            disposition: moa_execution::wire::execution_failure_disposition(status)
                .map_err(|error| TerminalError::new(error.to_string()))?,
            summary: delivery.summary.clone(),
        },
        status => {
            return Err(TerminalError::new_with_code(
                422,
                format!(
                    "execution terminal handler received nonterminal status {}",
                    status.as_str()
                ),
            )
            .into());
        }
    };
    append_exact_execution_event(ctx, terminal_event, format!("execution-terminal:{run_uid}"))
        .await?;

    let requested = ExecutionSynthesisRequested {
        run_uid,
        originating_user_sequence_num: origin,
        turn_id: stable_execution_synthesis_turn_id(run_uid, origin),
        terminal: delivery.summary,
        run_evidence: ExecutionRunEvidenceRef::ExecutionRun { run_uid },
    };
    append_exact_execution_event(
        ctx,
        Event::ExecutionSynthesisRequested(requested.clone()),
        format!("execution-synthesis:{run_uid}:{origin}"),
    )
    .await?;
    Ok(requested)
}

fn stable_execution_synthesis_turn_id(run_uid: uuid::Uuid, origin: u64) -> String {
    let name = format!("{EXECUTION_SYNTHESIS_TURN_DOMAIN}:{run_uid}:{origin}");
    uuid::Uuid::new_v5(&EXECUTION_SYNTHESIS_TURN_NAMESPACE, name.as_bytes()).to_string()
}

/// Publishes one aggregate progress event when both Session-owned gates pass.
pub(super) async fn accept_execution_progress(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    progress: ExecutionProgress,
    progress_interval_ms: u64,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx).await?;
    if !state
        .apply_execution_progress(progress.clone(), now, progress_interval_ms)
        .map_err(crate::workflows::errors::moa_error_to_status_handler_error)?
    {
        return Ok(());
    }
    append_exact_execution_event(
        ctx,
        Event::ExecutionProgress(progress.clone()),
        format!(
            "execution-progress:{}:{}:{}:{}:{}:{}:{}",
            progress.run_uid,
            progress.plan_revision,
            progress.status,
            progress.total,
            progress.completed,
            progress.failed,
            progress.cancelled,
        ),
    )
    .await
}

/// Publishes and activates one exact waiting execution task input request.
pub(super) async fn accept_execution_input_required(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    input: ExecutionInputRequired,
) -> Result<(), HandlerError> {
    let Some(run) = state
        .active_execution_runs
        .iter()
        .find(|run| run.run_uid == input.run_uid)
    else {
        return Err(TerminalError::new_with_code(
            409,
            "execution input references an inactive run",
        )
        .into());
    };
    if run.originating_user_sequence_num != input.originating_user_sequence_num {
        return Err(TerminalError::new_with_code(
            409,
            "execution input origin conflicts with admitted run",
        )
        .into());
    }
    if input.generation == 0 || input.question.trim().is_empty() {
        return Err(TerminalError::new_with_code(
            422,
            "execution input requires a nonzero generation and nonempty question",
        )
        .into());
    }
    append_exact_execution_event(
        ctx,
        Event::ExecutionInputRequired(input.clone()),
        format!(
            "execution-input:{}:{}:{}",
            input.run_uid, input.task_id, input.generation
        ),
    )
    .await?;
    state.upsert_pending_user_reply_target(PendingUserReplyTarget::ExecutionInput {
        run_uid: input.run_uid,
        task_id: input.task_id,
        generation: input.generation,
    });
    Ok(())
}

async fn append_exact_execution_event(
    ctx: &ObjectContext<'_>,
    event: Event,
    dedupe_key: String,
) -> Result<(), HandlerError> {
    let session_id = parse_session_key(ctx.key())?;
    let persisted = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: event.clone(),
                dedupe_key: Some(dedupe_key),
            })),
    )
    .call()
    .await?
    .into_inner();
    if persisted.event != event {
        return Err(TerminalError::new_with_code(
            409,
            "execution event replay conflicts with first persisted evidence",
        )
        .into());
    }
    Ok(())
}

/// Validates, persists, and activates one committed execution-run start payload.
pub(super) async fn accept_execution_run_started(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    started: ExecutionRunStarted,
    approved_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
) -> Result<(), HandlerError> {
    let identity = crate::handlers::authz_shim::require_identity(ctx)?;
    started
        .validate()
        .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
    state
        .ensure_initialized()
        .map_err(crate::workflows::errors::moa_error_to_status_handler_error)?;
    if state.owning_identity.is_none() {
        state.owning_identity = Some(identity.clone());
    }
    let session_id = parse_session_key(ctx.key())?;
    let records_call = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_events(Json(GetEventsRequest {
            session_id,
            range: EventRange {
                from_seq: Some(started.originating_user_sequence_num),
                to_seq: Some(started.originating_user_sequence_num),
                event_types: None,
                limit: Some(1),
            },
        }));
    let records = with_identity_headers(records_call, &identity)
        .call()
        .await?
        .into_inner();
    if !matches!(
        records.as_slice(),
        [EventRecord {
            event: Event::UserMessage { .. },
            ..
        }]
    ) {
        return Err(TerminalError::new_with_code(
            409,
            "execution run origin is not an exact persisted user message",
        )
        .into());
    }

    let persisted = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: Event::ExecutionRunStarted(started.clone()),
                dedupe_key: Some(format!("execution-run-started:{}", started.run_uid)),
            })),
    )
    .call()
    .await?
    .into_inner();
    if persisted.event != Event::ExecutionRunStarted(started.clone()) {
        return Err(TerminalError::new_with_code(
            409,
            "execution run started replay conflicts with first persisted evidence",
        )
        .into());
    }

    if state
        .execution_synthesis_marker(started.run_uid, started.originating_user_sequence_num)
        .is_some()
    {
        return Ok(());
    }
    if let Some(existing) = state
        .active_execution_runs
        .iter()
        .find(|existing| existing.run_uid == started.run_uid)
    {
        if existing.originating_user_sequence_num != started.originating_user_sequence_num {
            return Err(TerminalError::new_with_code(
                409,
                "active execution run marker conflicts with persisted start evidence",
            )
            .into());
        }
        return Ok(());
    }
    if let Some(confirmation) = started.confirmation.as_ref() {
        let hash = confirmation
            .active_plan_hash
            .parse::<moa_execution::capability::ExecutionHash>()
            .map_err(|error| TerminalError::new_with_code(422, error.to_string()))?;
        state.upsert_pending_user_reply_target(PendingUserReplyTarget::ExecutionConfirmation {
            run_uid: started.run_uid,
            expected_plan_hash: *hash.as_bytes(),
            approved_budget,
        });
    }
    state.active_execution_runs.push(ActiveExecutionRunState {
        run_uid: started.run_uid,
        originating_user_sequence_num: started.originating_user_sequence_num,
        progress: None,
        last_progress_signature: None,
        last_progress_at: None,
    });
    state
        .active_execution_runs
        .sort_by_key(|marker| marker.run_uid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::stable_execution_synthesis_turn_id;

    #[test]
    fn synthesis_turn_id_is_deterministic_uuid_scoped_to_run_and_origin() {
        // Pins: a synthesis workflow key satisfies the UUID turn contract while replaying
        // exactly for one run+origin and remaining distinct for different inputs.
        let run_uid = uuid::Uuid::from_u128(41);
        let stable = stable_execution_synthesis_turn_id(run_uid, 7);

        assert_eq!(
            uuid::Uuid::parse_str(&stable).map(|id| id.to_string()),
            Ok(stable.clone())
        );
        assert_eq!(stable_execution_synthesis_turn_id(run_uid, 7), stable);
        assert_ne!(stable_execution_synthesis_turn_id(run_uid, 8), stable);
        assert_ne!(
            stable_execution_synthesis_turn_id(uuid::Uuid::from_u128(42), 7),
            stable
        );
    }
}
