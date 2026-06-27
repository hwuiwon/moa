//! Target execution paths for behavior-lab trial workflows.

use super::status::{
    attach_trial_session, attach_trial_workflow_run, increment_trial_turn,
    status_response_from_record, stop_trial,
};
use super::trial_simulator::{SimulatorContext, simulator_done, simulator_next_user_message};
use super::*;

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
struct StartedWorkflowRun {
    run_uid: Uuid,
    trial: ExperimentTrialRecord,
    stop: Option<WorkflowTrialStop>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkflowTrialStart {
    scope: ActionRuleScope,
    trial_uid: Uuid,
    workflow_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
    tenant_id: TenantId,
    identity: Identity,
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

    let initial_events = load_session_events(ctx, session_id, EventRange::all()).await?;
    let mut transcript = transcript_from_events(&initial_events);
    let mut transcript_sequence = latest_sequence(&initial_events);
    let mut target_usage_sequence = transcript_sequence;
    for turn_index in trial.turn_count.max(0) as u32..simulator_context.max_turns {
        let observation = observe_session_after(ctx, session_id, transcript_sequence).await?;
        if let Some(stop) = stop_for_session_status(&observation.status) {
            return stop_trial(
                ctx,
                request.tenant_id,
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
        transcript_sequence = observation.latest_sequence;

        let simulator_message =
            simulator_next_user_message(ctx, &trial, &simulator_context, &transcript, turn_index)
                .await?;
        if simulator_done(&simulator_message) {
            return stop_trial(
                ctx,
                request.tenant_id,
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
                    contact: None,
                    max_turns: None,
                })),
            &request.identity,
        )
        .call()
        .await?;
        increment_trial_turn(ctx, request.tenant_id, trial.trial_uid).await?;
        transcript.push(ContextMessage::user(simulator_message));

        let status = wait_for_target_after_turn(ctx, session_id, &request.identity).await?;
        record_target_usage_after(ctx, session_id, &mut target_usage_sequence).await?;
        if let Some(stop) = stop_for_session_status(&status) {
            return stop_trial(
                ctx,
                request.tenant_id,
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
        request.tenant_id,
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

    let scope = tenant_scope(request.tenant_id);
    let session_id = match trial.session_id.or(target_session_id) {
        Some(session_id) => session_id,
        None => {
            let model = target_model
                .clone()
                .ok_or_else(|| bad_request("agent-loop trial requires a target model"))?;
            let agent = target_agent.ok_or_else(|| {
                bad_request("agent-loop simulator target requires an agent selector")
            })?;
            let (session_id, meta) =
                create_new_session(ctx, request.tenant_id, model, &request.identity, agent).await?;
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

    let scope = tenant_scope(request.tenant_id);
    let run = start_and_attach_workflow_run(
        ctx,
        WorkflowTrialStart {
            scope,
            trial_uid: trial.trial_uid,
            workflow_ref,
            input,
            session_id,
            idempotency_key: idempotency_key.or_else(|| Some(trial.trial_key.clone())),
            tenant_id: request.tenant_id,
            identity: request.identity.clone(),
        },
    )
    .await?;
    ctx.set(K_WORKFLOW_RUN_UID, Json(run.run_uid));
    tracing::Span::current()
        .set_attribute("moa.experiment.workflow_run_uid", run.run_uid.to_string());

    if let Some(stop) = run.stop {
        return stop_trial(
            ctx,
            request.tenant_id,
            trial.trial_uid,
            stop.status,
            stop.stop_reason,
            run.error,
        )
        .await;
    }

    status_response_from_record(request.tenant_id, run.trial)
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    identity: &Identity,
    agent: AgentSessionSelection,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = OrchestratorCtx::current().session_store_backend();
    let pool = OrchestratorCtx::current_graph_pool();
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
    session_id: SessionId,
    sequence_num: u64,
) -> Result<TargetObservation, HandlerError> {
    let status = ctx
        .object_client::<SessionClient>(session_id.to_string())
        .status()
        .call()
        .await?
        .into_inner();
    let events = load_session_events(ctx, session_id, event_range_after(sequence_num)).await?;
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
) -> Result<Vec<EventRecord>, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
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
    session_id: SessionId,
    identity: &Identity,
) -> Result<SessionStatus, HandlerError> {
    for _ in 0..TARGET_WAIT_ATTEMPTS {
        let status = ctx
            .object_client::<SessionClient>(session_id.to_string())
            .status()
            .call()
            .await?
            .into_inner();
        let snapshot = with_identity_headers(
            ctx.object_client::<SessionClient>(session_id.to_string())
                .snapshot(),
            identity,
        )
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

async fn record_target_usage_after(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    sequence_num: &mut u64,
) -> Result<(), HandlerError> {
    let store = OrchestratorCtx::current_session_store();
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

fn target_is_waiting_or_idle(status: &SessionStatus, snapshot: &SessionSnapshot) -> bool {
    matches!(
        status,
        SessionStatus::Paused
            | SessionStatus::Completed
            | SessionStatus::Cancelled
            | SessionStatus::Failed
    ) && snapshot.active_turn_id.is_none()
}

async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    start: WorkflowTrialStart,
) -> Result<StartedWorkflowRun, HandlerError> {
    let pool = OrchestratorCtx::current_graph_pool();
    let session_id = start.session_id;
    let tenant_id = start.tenant_id;
    let identity = start.identity.clone();
    let run = ctx
        .run(|| async move {
            let run = workflow_runtime(pool.clone())
                .start(
                    &start.scope,
                    StartWorkflowRun {
                        workflow_ref: start.workflow_ref,
                        input: start.input,
                        session_id: start.session_id,
                        idempotency_key: start.idempotency_key,
                    },
                )
                .await
                .map_err(workflow_handler_error)?;
            let trial =
                attach_trial_workflow_run(pool, start.scope, start.trial_uid, run.run_uid).await?;
            let stop = trial_stop_for_workflow_status(&run.status).map(|(status, stop_reason)| {
                WorkflowTrialStop {
                    status,
                    stop_reason,
                }
            });
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
                trial,
                stop,
                error: run.error,
            }))
        })
        .name("experiment_trial_start_workflow_run")
        .await?
        .into_inner();
    ctx.workflow_client::<ArtifactWorkflowExecutionClient>(run.run_uid.to_string())
        .run(Json::from(RunArtifactWorkflowRequest {
            tenant_id,
            run_uid: run.run_uid,
            identity,
            session_id,
        }))
        .send();
    Ok(run)
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

fn trial_stop_for_workflow_status(
    status: &ArtifactRunStatus,
) -> Option<(ExperimentTrialStatus, ExperimentTrialStopReason)> {
    match status {
        ArtifactRunStatus::Queued
        | ArtifactRunStatus::Running
        | ArtifactRunStatus::PendingReview => None,
        ArtifactRunStatus::Completed => Some((
            ExperimentTrialStatus::Completed,
            ExperimentTrialStopReason::TargetTerminal,
        )),
        ArtifactRunStatus::Failed => Some((
            ExperimentTrialStatus::Failed,
            ExperimentTrialStopReason::Error,
        )),
        ArtifactRunStatus::Cancelled => Some((
            ExperimentTrialStatus::Cancelled,
            ExperimentTrialStopReason::Cancelled,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::{EventRecord, MessageRole, ModelTier};

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
    fn workflow_status_stops_trials_only_after_terminal_states_offline() {
        // Pins: workflow-backed trials do not report success while target work is still active.
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::Queued),
            None
        );
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::Running),
            None
        );
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::PendingReview),
            None
        );
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::Completed),
            Some((
                ExperimentTrialStatus::Completed,
                ExperimentTrialStopReason::TargetTerminal,
            ))
        );
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::Failed),
            Some((
                ExperimentTrialStatus::Failed,
                ExperimentTrialStopReason::Error,
            ))
        );
        assert_eq!(
            trial_stop_for_workflow_status(&ArtifactRunStatus::Cancelled),
            Some((
                ExperimentTrialStatus::Cancelled,
                ExperimentTrialStopReason::Cancelled,
            ))
        );
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
