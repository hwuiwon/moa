//! Status projection helpers for behavior-lab experiment runs.

use super::plan_expansion::{aggregate_plan_status_from_store, plan_revision_uid_from_run};
use super::*;

pub(super) async fn status_response(
    pool: sqlx::PgPool,
    request: ExperimentRunStatusRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let tenant_id = request.tenant_id;
    let store = ExperimentStore::new(pool.clone());
    let mut run = store
        .load_run_for_workflow(tenant_id, request.run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(request.run_uid))?;
    let scope = run.scope;

    plan_revision_uid_from_run(&run).ok_or_else(|| {
        TerminalError::new("experiment run is missing its required plan revision")
    })?;
    let aggregate = aggregate_plan_status_from_store(pool, scope, run.run_uid).await?;
    if aggregate.status != run.status || aggregate.error != run.error {
        run.status = aggregate.status;
        run.error = aggregate.error;
    }
    status_response_from_record(tenant_id, run)
}

pub(super) async fn run_status_response(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunStatusRequest,
    pool: &sqlx::PgPool,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let pool = pool.clone();
    Ok(ctx
        .run(|| async move { status_response(pool, request).await.map(Json::from) })
        .name("experiment_run_response")
        .await?
        .into_inner())
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
        execution_run_uid: run.execution_run_uid,
        error: run.error,
        run: run_value,
    })
}
