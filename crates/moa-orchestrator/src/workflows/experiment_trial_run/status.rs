//! Persistence and status projection helpers for behavior-lab trial workflows.

use super::*;

pub(super) async fn insert_or_load_trial(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trial: NewExperimentTrial,
) -> Result<ExperimentTrialRecord, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    Ok(ctx
        .run(|| async move {
            ExperimentStore::new(pool)
                .insert_trial(&scope, trial)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("experiment_trial_insert_or_load")
        .await?
        .into_inner())
}

pub(super) async fn persist_trial_status(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trial_uid: Uuid,
    status: ExperimentTrialStatus,
    stop_reason: Option<ExperimentTrialStopReason>,
    error: Option<String>,
) -> Result<ExperimentTrialRecord, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    Ok(ctx
        .run(|| async move {
            update_trial_status(pool, scope, trial_uid, status, stop_reason, error)
                .await
                .map(Json::from)
        })
        .name("experiment_trial_update_status")
        .await?
        .into_inner())
}

pub(super) async fn persist_trial_status_by_key(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    run_uid: Uuid,
    trial_key: String,
    status: ExperimentTrialStatus,
    stop_reason: Option<ExperimentTrialStopReason>,
    error: Option<String>,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    ctx.run(|| async move {
        let store = ExperimentStore::new(pool.clone());
        if let Some(trial) = store
            .load_trial_by_key(&scope, run_uid, &trial_key)
            .await
            .map_err(moa_error_to_handler_error)?
        {
            update_trial_status(pool, scope, trial.trial_uid, status, stop_reason, error).await?;
        }
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_trial_update_status_by_key")
    .await?;
    Ok(())
}

pub(super) async fn stop_trial(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trial_uid: Uuid,
    status: ExperimentTrialStatus,
    stop_reason: ExperimentTrialStopReason,
    error: Option<String>,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let trial = persist_trial_status(
        ctx,
        workspace_id.clone(),
        trial_uid,
        status,
        Some(stop_reason),
        error,
    )
    .await?;
    ctx.set(K_STATUS, Json(trial.status));
    status_response_from_record(workspace_id, trial)
}

pub(super) async fn increment_trial_turn(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trial_uid: Uuid,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    ctx.run(|| async move {
        ExperimentStore::new(pool)
            .increment_trial_turn(&scope, trial_uid)
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| trial_not_found(trial_uid))?;
        record_simulation_turn("agent_loop");
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_trial_increment_turn")
    .await?;
    Ok(())
}

pub(super) async fn attach_trial_session(
    ctx: &WorkflowContext<'_>,
    scope: MemoryScope,
    trial_uid: Uuid,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    ctx.run(|| async move {
        ExperimentStore::new(pool)
            .attach_trial_session(&scope, trial_uid, session_id)
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| trial_not_found(trial_uid))?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_trial_attach_session")
    .await?;
    Ok(())
}

pub(super) async fn attach_current_trial_trace(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    trial_uid: Uuid,
) -> Result<(), HandlerError> {
    let Some(trace_id) = current_trace_id() else {
        return Ok(());
    };
    tracing::Span::current().set_attribute("moa.experiment.trace_id", trace_id.clone());
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    ctx.run(|| async move {
        ExperimentStore::new(pool)
            .attach_trial_trace(&scope, trial_uid, trace_id)
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| trial_not_found(trial_uid))?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_trial_attach_trace")
    .await?;
    Ok(())
}

pub(super) async fn attach_trial_workflow_run(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    trial_uid: Uuid,
    workflow_run_uid: Uuid,
) -> Result<ExperimentTrialRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_trial_workflow_run(&scope, trial_uid, workflow_run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| trial_not_found(trial_uid))
}

async fn update_trial_status(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    trial_uid: Uuid,
    status: ExperimentTrialStatus,
    stop_reason: Option<ExperimentTrialStopReason>,
    error: Option<String>,
) -> Result<ExperimentTrialRecord, HandlerError> {
    let completed_at = completed_at_for_status(status);
    let trial = ExperimentStore::new(pool)
        .update_trial_status(&scope, trial_uid, status, stop_reason, error, completed_at)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| trial_not_found(trial_uid))?;
    record_trial_metrics(&trial);
    Ok(trial)
}

pub(super) async fn trial_status_response(
    pool: sqlx::PgPool,
    request: ExperimentTrialRunStatusRequest,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let trial = ExperimentStore::new(pool)
        .load_trial(&scope, request.trial_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| trial_not_found(request.trial_uid))?;
    status_response_from_record(request.workspace_id, trial)
}

pub(super) fn status_response_from_record(
    workspace_id: WorkspaceId,
    trial: ExperimentTrialRecord,
) -> Result<ExperimentTrialRunStatusResponse, HandlerError> {
    let trial_value = serde_json::to_value(&trial).map_err(|error| {
        TerminalError::new(format!("serialize experiment trial failed: {error}"))
    })?;
    Ok(ExperimentTrialRunStatusResponse {
        workspace_id,
        run_uid: trial.run_uid,
        trial_uid: trial.trial_uid,
        trial_key: trial.trial_key,
        status: trial.status.as_str().to_string(),
        target_kind: trial.target_kind.as_str().to_string(),
        stop_reason: trial.stop_reason.map(|reason| reason.as_str().to_string()),
        turn_count: trial.turn_count,
        session_id: trial.session_id,
        workflow_run_uid: trial.workflow_run_uid,
        score_run_id: trial.score_run_id,
        error: trial.error,
        trial: trial_value,
    })
}

pub(super) fn trial_status_allows_child_start(status: ExperimentTrialStatus) -> bool {
    matches!(
        status,
        ExperimentTrialStatus::Accepted | ExperimentTrialStatus::Dispatched
    )
}

fn completed_at_for_status(status: ExperimentTrialStatus) -> Option<chrono::DateTime<Utc>> {
    if status.is_terminal() {
        Some(Utc::now())
    } else {
        None
    }
}

fn record_trial_metrics(trial: &ExperimentTrialRecord) {
    record_experiment_trial(
        trial.status.as_str(),
        trial.stop_reason.map(ExperimentTrialStopReason::as_str),
        trial.target_kind.as_str(),
    );
    if let (Some(started_at), Some(completed_at)) = (trial.started_at, trial.completed_at)
        && let Ok(duration) = completed_at.signed_duration_since(started_at).to_std()
    {
        record_experiment_trial_duration(
            trial.target_kind.as_str(),
            trial.status.as_str(),
            duration,
        );
    }
}
