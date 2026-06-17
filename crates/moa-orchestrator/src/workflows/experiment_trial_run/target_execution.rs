//! Target execution paths for behavior-lab trial workflows.

use super::status::{
    attach_trial_session, attach_trial_workflow_run, increment_trial_turn, stop_trial,
};
use super::trial_simulator::{SimulatorContext, simulator_done, simulator_next_user_message};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetObservation {
    status: SessionStatus,
    latest_response: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartedWorkflowRun {
    run_uid: Uuid,
    trial_status: ExperimentTrialStatus,
    stop_reason: ExperimentTrialStopReason,
    error: Option<String>,
}

pub(super) async fn run_agent_loop_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    trial: ExperimentTrialRecord,
    simulator_context: SimulatorContext,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target.clone())?;
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let (session_id, target_model) =
        ensure_agent_loop_session(ctx, &request, &trial, target, variant).await?;
    ctx.set(K_SESSION_ID, Json(session_id));
    tracing::Span::current().set_attribute("moa.experiment.session_id", session_id.to_string());

    let mut transcript = Vec::new();
    let mut target_event_offset = load_session_events(ctx, session_id).await?.len();
    for turn_index in trial.turn_count.max(0) as u32..simulator_context.max_turns {
        let observation = observe_session(ctx, session_id).await?;
        if let Some(stop) = stop_for_session_status(&observation.status) {
            return stop_trial(
                ctx,
                request.workspace_id,
                trial.trial_uid,
                stop.0,
                stop.1,
                None,
            )
            .await;
        }
        if let Some(response) = observation.latest_response {
            transcript.push(ContextMessage::assistant(format!(
                "Target response: {response}"
            )));
        }

        let simulator_message =
            simulator_next_user_message(ctx, &trial, &simulator_context, &transcript, turn_index)
                .await?;
        if simulator_done(&simulator_message) {
            return stop_trial(
                ctx,
                request.workspace_id,
                trial.trial_uid,
                ExperimentTrialStatus::Completed,
                ExperimentTrialStopReason::SimulatorDone,
                None,
            )
            .await;
        }

        with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .queue_message(Json::from(QueueMessageRequest {
                    user_message: simulator_message.clone(),
                    attachments: Vec::new(),
                    model: target_model.as_ref().map(ToString::to_string),
                })),
            &request.identity,
        )
        .call()
        .await?;
        increment_trial_turn(ctx, request.workspace_id.clone(), trial.trial_uid).await?;
        record_simulation_turn(trial.target_kind.as_str());
        transcript.push(ContextMessage::user(simulator_message));

        let status = wait_for_target_after_turn(ctx, session_id).await?;
        record_target_usage_since(ctx, session_id, &mut target_event_offset).await?;
        if let Some(stop) = stop_for_session_status(&status) {
            return stop_trial(
                ctx,
                request.workspace_id,
                trial.trial_uid,
                stop.0,
                stop.1,
                None,
            )
            .await;
        }
    }

    stop_trial(
        ctx,
        request.workspace_id,
        trial.trial_uid,
        ExperimentTrialStatus::Completed,
        ExperimentTrialStopReason::MaxTurns,
        None,
    )
    .await
}

async fn ensure_agent_loop_session(
    ctx: &WorkflowContext<'_>,
    request: &ExperimentTrialRunWorkflowRequest,
    trial: &ExperimentTrialRecord,
    target: ExperimentTarget,
    variant: ExperimentVariant,
) -> Result<(SessionId, Option<ModelId>), HandlerError> {
    let (target_session_id, target_model, attachments_empty) = match target {
        ExperimentTarget::AgentLoop {
            session_id,
            model,
            attachments,
            ..
        } => (
            session_id,
            trial.target_model.clone().or(variant.model).or(Some(model)),
            attachments.is_empty(),
        ),
        ExperimentTarget::Workflow { .. } => {
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

    let scope = workspace_scope(request.workspace_id.clone());
    let session_id = match trial.session_id.or(target_session_id) {
        Some(session_id) => session_id,
        None => {
            let model = target_model
                .clone()
                .ok_or_else(|| bad_request("agent-loop trial requires a target model"))?;
            let (session_id, meta) =
                create_new_session(ctx, request.workspace_id.clone(), model, &request.identity)
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
    attach_trial_session(ctx, scope, trial.trial_uid, session_id).await?;
    Ok((session_id, target_model))
}

pub(super) async fn run_workflow_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    trial: ExperimentTrialRecord,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target)?;
    let ExperimentTarget::Workflow {
        workflow_ref,
        input,
        session_id,
        idempotency_key,
    } = target
    else {
        return Err(bad_request(
            "workflow trial received an agent-loop experiment target",
        ));
    };

    let scope = workspace_scope(request.workspace_id.clone());
    let run = start_and_attach_workflow_run(
        ctx,
        scope,
        trial.trial_uid,
        workflow_ref,
        input,
        session_id,
        idempotency_key.or_else(|| Some(trial.trial_key.clone())),
    )
    .await?;
    ctx.set(K_WORKFLOW_RUN_UID, Json(run.run_uid));
    tracing::Span::current()
        .set_attribute("moa.experiment.workflow_run_uid", run.run_uid.to_string());

    stop_trial(
        ctx,
        request.workspace_id,
        trial.trial_uid,
        run.trial_status,
        run.stop_reason,
        run.error,
    )
    .await
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    model: ModelId,
    identity: &Identity,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = OrchestratorCtx::current().session_store.clone();
    let identity = identity.clone();
    Ok(ctx
        .run(|| async move {
            let meta = new_session_meta(workspace_id, model, &identity)?;
            let session_id = create_session_for_identity(store.as_ref(), meta.clone(), identity)
                .await
                .map_err(non_retryable_handler_error)?;
            Ok::<_, HandlerError>(Json::from((session_id, meta)))
        })
        .name("experiment_trial_create_session")
        .await?
        .into_inner())
}

async fn observe_session(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<TargetObservation, HandlerError> {
    let status = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .status()
        .call()
        .await?
        .into_inner();
    let events = load_session_events(ctx, session_id).await?;
    Ok(TargetObservation {
        status,
        latest_response: latest_brain_response(&events),
    })
}

async fn load_session_events(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current().session_store.clone();
    Ok(ctx
        .run(|| async move {
            store
                .get_events(session_id, EventRange::all())
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
    session_id: SessionId,
) -> Result<SessionStatus, HandlerError> {
    for _ in 0..TARGET_WAIT_ATTEMPTS {
        let status = ctx
            .object_client::<SessionClient>(session_id.to_string())
            .status()
            .call()
            .await?
            .into_inner();
        let snapshot = ctx
            .object_client::<SessionClient>(session_id.to_string())
            .snapshot()
            .call()
            .await?
            .into_inner();
        if target_is_waiting_or_idle(&status, &snapshot) {
            return Ok(status);
        }
        ctx.sleep(TARGET_WAIT_INTERVAL).await?;
    }

    Err(TerminalError::new("timed out waiting for target session turn").into())
}

async fn record_target_usage_since(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event_offset: &mut usize,
) -> Result<(), HandlerError> {
    let events = load_session_events(ctx, session_id).await?;
    let (tokens, cost_cents) = target_usage_from_events(&events, *event_offset);
    *event_offset = events.len();
    record_simulation_tokens("target", tokens);
    record_simulation_cost_cents("target", cost_cents);
    Ok(())
}

fn target_usage_from_events(events: &[EventRecord], event_offset: usize) -> (u64, u64) {
    events
        .iter()
        .skip(event_offset)
        .fold((0_u64, 0_u64), |(tokens, cost_cents), record| {
            (
                tokens + (record.event.input_tokens() + record.event.output_tokens()) as u64,
                cost_cents + u64::from(record.event.cost_cents()),
            )
        })
}

fn target_is_waiting_or_idle(status: &SessionStatus, snapshot: &SessionSnapshot) -> bool {
    matches!(
        status,
        SessionStatus::Paused
            | SessionStatus::Completed
            | SessionStatus::WaitingApproval
            | SessionStatus::Cancelled
            | SessionStatus::Failed
    ) && snapshot.active_turn_id.is_none()
}

async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    scope: MemoryScope,
    trial_uid: Uuid,
    workflow_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
) -> Result<StartedWorkflowRun, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    Ok(ctx
        .run(|| async move {
            let run = workflow_runtime(pool.clone())
                .start(
                    &scope,
                    StartWorkflowRun {
                        workflow_ref,
                        input,
                        session_id,
                        idempotency_key,
                    },
                )
                .await
                .map_err(workflow_handler_error)?;
            attach_trial_workflow_run(pool, scope, trial_uid, run.run_uid).await?;
            let (trial_status, stop_reason) = trial_status_from_workflow_status(&run.status);
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
                trial_status,
                stop_reason,
                error: run.error,
            }))
        })
        .name("experiment_trial_start_workflow_run")
        .await?
        .into_inner())
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
        SessionStatus::WaitingApproval => Some((
            ExperimentTrialStatus::WaitingApproval,
            ExperimentTrialStopReason::ApprovalWait,
        )),
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

fn trial_status_from_workflow_status(
    status: &ArtifactRunStatus,
) -> (ExperimentTrialStatus, ExperimentTrialStopReason) {
    match status {
        ArtifactRunStatus::WaitingApproval => (
            ExperimentTrialStatus::WaitingApproval,
            ExperimentTrialStopReason::ApprovalWait,
        ),
        ArtifactRunStatus::Failed => (
            ExperimentTrialStatus::Failed,
            ExperimentTrialStopReason::Error,
        ),
        ArtifactRunStatus::Cancelled => (
            ExperimentTrialStatus::Cancelled,
            ExperimentTrialStopReason::Cancelled,
        ),
        ArtifactRunStatus::Queued | ArtifactRunStatus::Running | ArtifactRunStatus::Completed => (
            ExperimentTrialStatus::Completed,
            ExperimentTrialStopReason::TargetTerminal,
        ),
    }
}
