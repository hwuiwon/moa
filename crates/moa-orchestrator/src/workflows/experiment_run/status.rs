//! Status projection helpers for behavior-lab experiment runs.

use super::plan_expansion::{aggregate_plan_status_from_store, plan_revision_uid_from_run};
use super::*;

pub(super) async fn status_response(
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
    request: ExperimentRunStatusRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let tenant_id = request.tenant_id;
    let scope = tenant_scope(tenant_id);
    let store = ExperimentStore::new(pool.clone());
    let mut run = store
        .load_run(&scope, request.run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(request.run_uid))?;

    if plan_revision_uid_from_run(&run).is_some() {
        let aggregate = aggregate_plan_status_from_store(pool.clone(), scope, run.run_uid).await?;
        if aggregate.status != run.status || aggregate.error != run.error {
            run.status = aggregate.status;
            run.error = aggregate.error;
        }
        return status_response_from_record(tenant_id, run);
    }

    if run.target_kind == ExperimentTargetKind::Procedure {
        return linked_workflow_status_response(pool, scope, tenant_id, run).await;
    }

    if let Some(status) =
        derived_session_status(run.status, run.session_id, session_store.as_ref()).await?
        && status != run.status
    {
        run.status = status;
    }

    status_response_from_record(tenant_id, run)
}

async fn linked_workflow_status_response(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    tenant_id: TenantId,
    mut run: ExperimentRunRecord,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let Some(procedure_run_uid) = run.procedure_run_uid else {
        return status_response_from_record(tenant_id, run);
    };

    let workflow_run = workflow_runtime(pool.clone())
        .status(&scope, procedure_run_uid)
        .await
        .map_err(procedure_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;

    if let Some(status) = experiment_status_from_artifact_status(&workflow_run.status)
        && status != run.status
    {
        run.status = status;
        run.error = workflow_run.error.clone();
        run.completed_at = workflow_run.completed_at;
    }

    let mut response = status_response_from_record_with_status(
        tenant_id,
        run,
        workflow_run.status.as_str().to_string(),
    )?;
    response.session_id = workflow_run.session_id.or(response.session_id);
    if workflow_run.error.is_some() {
        response.error = workflow_run.error;
    }
    Ok(response)
}

pub(super) async fn procedure_status_response(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunStatusRequest,
    pool: &sqlx::PgPool,
    session_store: &Arc<PostgresSessionStore>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let pool = pool.clone();
    let session_store = session_store.clone();
    Ok(ctx
        .run(|| async move {
            status_response(pool, session_store, request)
                .await
                .map(Json::from)
        })
        .name("experiment_run_response")
        .await?
        .into_inner())
}

async fn derived_session_status(
    row_status: ExperimentRunStatus,
    session_id: Option<SessionId>,
    session_store: &PostgresSessionStore,
) -> Result<Option<ExperimentRunStatus>, HandlerError> {
    if matches!(
        row_status,
        ExperimentRunStatus::Completed
            | ExperimentRunStatus::Failed
            | ExperimentRunStatus::Cancelled
    ) {
        return Ok(Some(row_status));
    }

    let Some(session_id) = session_id else {
        return Ok(Some(row_status));
    };
    let session = session_store
        .get_session(session_id)
        .await
        .map_err(moa_error_to_handler_error)?;
    Ok(Some(match session.status {
        SessionStatus::Created => row_status,
        SessionStatus::Running => ExperimentRunStatus::Running,
        SessionStatus::Paused | SessionStatus::Completed => ExperimentRunStatus::Completed,
        SessionStatus::Cancelled => ExperimentRunStatus::Cancelled,
        SessionStatus::Failed => ExperimentRunStatus::Failed,
    }))
}

fn experiment_status_from_artifact_status(
    status: &ArtifactRunStatus,
) -> Option<ExperimentRunStatus> {
    match status {
        ArtifactRunStatus::Queued => None,
        ArtifactRunStatus::Running | ArtifactRunStatus::PendingReview => {
            Some(ExperimentRunStatus::Running)
        }
        ArtifactRunStatus::Completed => Some(ExperimentRunStatus::Completed),
        ArtifactRunStatus::Failed => Some(ExperimentRunStatus::Failed),
        ArtifactRunStatus::Cancelled => Some(ExperimentRunStatus::Cancelled),
    }
}

fn status_response_from_record(
    tenant_id: TenantId,
    run: ExperimentRunRecord,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let status = run.status.as_str().to_string();
    status_response_from_record_with_status(tenant_id, run, status)
}

fn status_response_from_record_with_status(
    tenant_id: TenantId,
    run: ExperimentRunRecord,
    status: String,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let run_value = serde_json::to_value(&run)
        .map_err(|error| TerminalError::new(format!("serialize experiment run failed: {error}")))?;
    Ok(ExperimentRunStatusResponse {
        tenant_id,
        run_uid: run.run_uid,
        status,
        target_kind: Some(run.target_kind.as_str().to_string()),
        score_run_id: Some(run.score_run_id),
        session_id: run.session_id,
        procedure_run_uid: run.procedure_run_uid,
        error: run.error,
        run: run_value,
    })
}
