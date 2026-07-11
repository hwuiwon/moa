//! Target execution paths for behavior-lab trial workflows.

use super::status::{
    attach_trial_procedure_run, attach_trial_session, increment_trial_turn, stop_trial,
};
use super::trial_simulator::{SimulatorContext, simulator_done, simulator_next_user_message};
use super::*;
use crate::objects::session::{AttachSessionTurnWaiterInput, RemoveSessionTurnWaiterInput};

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
    stop: Option<WorkflowTrialStop>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct WorkflowTrialStart {
    scope: ActionRuleScope,
    trial_uid: Uuid,
    procedure_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
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
                request.tenant_id,
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
                request.tenant_id,
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
        increment_trial_turn(ctx, request.tenant_id, trial.trial_uid, pool).await?;
        transcript.push(ContextMessage::user(simulator_message));

        let status =
            wait_for_target_after_turn(ctx, &request.identity, session_id, turn_id).await?;
        record_target_usage_after(ctx, session_id, &mut target_usage_sequence, session_store)
            .await?;
        if let Some(stop) = stop_for_session_status(&status) {
            return stop_trial(
                ctx,
                request.tenant_id,
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
        request.tenant_id,
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
        ExperimentTarget::Procedure { .. } => {
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

/// Executes one procedure-backed behavior-lab trial and durably waits for the
/// procedure to reach a terminal state before returning.
///
/// The trial starts the durable procedure run row, then invokes the
/// [`ProcedureExecution`](crate::workflows::procedure_execution::ProcedureExecution)
/// `run` handler with a durable request-response `.call()` raced against
/// [`TARGET_WAIT_TIMEOUT`] via `restate_sdk::select!`. This mirrors the agent-loop
/// turn wait in [`wait_for_target_after_turn`] so the trial only resolves its
/// parent's completion awakeable once the procedure has actually finished — the
/// prior fire-and-forget `.send()` let the parent fan-in proceed while the
/// procedure was still executing.
///
/// The procedure `run` handler blocks internally while a run is paused on a
/// `Review` or `WaitSignal` node, so a procedure awaiting human review never
/// resolves the call and the trial times out instead. That is intentional for
/// behavior-lab semantics: an experiment procedure that needs human review times
/// the trial out (recorded as `Failed`/`Error`, matching an agent-loop turn
/// timeout) rather than reporting a premature terminal status.
pub(super) async fn run_procedure_trial(
    ctx: &WorkflowContext<'_>,
    request: ExperimentTrialRunWorkflowRequest,
    trial: ExperimentTrialRecord,
    pool: &sqlx::PgPool,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target)?;
    let ExperimentTarget::Procedure {
        procedure_ref,
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
            procedure_ref,
            input,
            session_id,
            idempotency_key: idempotency_key.or_else(|| Some(trial.trial_key.clone())),
        },
        pool,
    )
    .await?;
    ctx.set(K_PROCEDURE_RUN_UID, Json(run.run_uid));
    tracing::Span::current()
        .set_attribute("moa.experiment.procedure_run_uid", run.run_uid.to_string());

    // Idempotent replay where the procedure was already terminal at start time
    // (for example a completed run matched by idempotency key): stop immediately
    // without re-invoking the executor.
    if let Some(stop) = run.stop {
        return stop_trial(
            ctx,
            request.tenant_id,
            trial.trial_uid,
            stop.status,
            stop.stop_reason,
            run.error,
            pool,
        )
        .await;
    }

    let (stop, error) = wait_for_procedure_outcome(
        ctx,
        request.tenant_id,
        request.identity.clone(),
        run.run_uid,
        session_id,
    )
    .await?;
    stop_trial(
        ctx,
        request.tenant_id,
        trial.trial_uid,
        stop.status,
        stop.stop_reason,
        error,
        pool,
    )
    .await
}

/// Durably waits for a procedure run to reach a terminal state and maps the
/// outcome into a trial stop.
///
/// Delegates the replay-safe race to
/// [`procedure_target_wait::wait_for_procedure_outcome`], which bounds the wait
/// by [`TARGET_WAIT_TIMEOUT`] exactly like the awakeable-vs-timer race in
/// [`wait_for_target_after_turn`]. A timeout, or an unexpected non-terminal
/// outcome, records the trial as `Failed`/`Error`, mirroring an agent-loop turn
/// timeout. On timeout the abandoned call keeps the child procedure invocation
/// running.
async fn wait_for_procedure_outcome(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    identity: Identity,
    run_uid: Uuid,
    session_id: Option<SessionId>,
) -> Result<(WorkflowTrialStop, Option<String>), HandlerError> {
    match procedure_target_wait::wait_for_procedure_outcome(
        ctx, tenant_id, identity, run_uid, session_id,
    )
    .await?
    {
        ProcedureWaitOutcome::Terminal(status, outcome) => {
            match trial_stop_for_workflow_status(&status) {
                Some((status, stop_reason)) => Ok((
                    WorkflowTrialStop {
                        status,
                        stop_reason,
                    },
                    outcome.error,
                )),
                None => Ok((
                    procedure_failure_stop(),
                    Some(format!(
                        "procedure run {run_uid} returned non-terminal status {}",
                        outcome.status
                    )),
                )),
            }
        }
        ProcedureWaitOutcome::NonTerminal(outcome) => Ok((
            procedure_failure_stop(),
            Some(format!(
                "procedure run {run_uid} returned non-terminal status {}",
                outcome.status
            )),
        )),
        ProcedureWaitOutcome::TimedOut => Ok((
            procedure_failure_stop(),
            Some(format!(
                "timed out waiting for procedure run {run_uid} to reach a terminal state"
            )),
        )),
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
        return Ok(status_for_turn_outcome(&outcome));
    }

    restate_sdk::select! {
        outcome = completion => {
            let outcome = parse_turn_outcome(&outcome?)?;
            Ok(status_for_turn_outcome(&outcome))
        },
        _ = ctx.sleep(TARGET_WAIT_TIMEOUT) => {
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

fn status_for_turn_outcome(outcome: &TurnOutcome) -> SessionStatus {
    match outcome.kind {
        TurnOutcomeKind::Completed => SessionStatus::Paused,
        TurnOutcomeKind::Cancelled => SessionStatus::Cancelled,
        TurnOutcomeKind::Failed => SessionStatus::Failed,
    }
}

/// Creates the durable procedure run row and links it to the trial.
///
/// This only writes durable state; the executor is invoked later by
/// [`wait_for_procedure_outcome`] so the trial can durably await the terminal
/// outcome instead of returning while the procedure runs.
async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    start: WorkflowTrialStart,
    pool: &sqlx::PgPool,
) -> Result<StartedWorkflowRun, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move {
            let run = workflow_runtime(pool.clone())
                .start(
                    &start.scope,
                    StartProcedureRun {
                        procedure_ref: start.procedure_ref,
                        input: start.input,
                        session_id: start.session_id,
                        idempotency_key: start.idempotency_key,
                    },
                )
                .await
                .map_err(procedure_handler_error)?;
            attach_trial_procedure_run(pool, start.scope, start.trial_uid, run.run_uid).await?;
            let stop = trial_stop_for_workflow_status(&run.status).map(|(status, stop_reason)| {
                WorkflowTrialStop {
                    status,
                    stop_reason,
                }
            });
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
                stop,
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

/// Trial stop recorded when a procedure trial fails to reach a terminal state in
/// time (timeout) or reports an unexpected non-terminal status. Mirrors the
/// agent-loop turn-timeout disposition (`Failed` / `Error`).
fn procedure_failure_stop() -> WorkflowTrialStop {
    WorkflowTrialStop {
        status: ExperimentTrialStatus::Failed,
        stop_reason: ExperimentTrialStopReason::Error,
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

    #[test]
    fn procedure_timeout_records_failed_error_stop_offline() {
        // Pins: a procedure trial that times out (for example blocked on human review) records
        // the same Failed/Error disposition as an agent-loop turn timeout.
        assert_eq!(
            procedure_failure_stop(),
            WorkflowTrialStop {
                status: ExperimentTrialStatus::Failed,
                stop_reason: ExperimentTrialStopReason::Error,
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
        assert_eq!(status_for_turn_outcome(&completed), SessionStatus::Paused);
        assert_eq!(status_for_turn_outcome(&failed), SessionStatus::Failed);
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
