//! Target execution paths for behavior-lab experiment runs.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartedWorkflowRun {
    run_uid: Uuid,
}

pub(super) async fn run_agent_loop_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    prompt: String,
    session_id: Option<SessionId>,
    model: ModelId,
    attachments: Vec<moa_core::Attachment>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant.clone())?;
    let scope = workspace_scope(request.workspace_id.clone());
    persist_run_status(
        ctx,
        request.workspace_id.clone(),
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
    )
    .await?;

    let model = variant.model.unwrap_or(model);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            let (session_id, meta) =
                create_new_session(ctx, request.workspace_id.clone(), model.clone(), &request)
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
    persist_attached_session(ctx, scope.clone(), request.run_uid, session_id).await?;

    with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .queue_message(Json::from(QueueMessageRequest {
                user_message: prompt,
                attachments,
                model: Some(model.to_string()),
            })),
        &request.identity,
    )
    .call()
    .await?;

    workflow_status_response(
        ctx,
        ExperimentRunStatusRequest {
            workspace_id: request.workspace_id,
            run_uid: request.run_uid,
        },
    )
    .await
}

pub(super) async fn run_workflow_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    workflow_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    persist_run_status(
        ctx,
        request.workspace_id.clone(),
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
    )
    .await?;

    let workflow_run = start_and_attach_workflow_run(
        ctx,
        scope,
        request.run_uid,
        workflow_ref,
        input,
        session_id,
        idempotency_key,
    )
    .await?;
    ctx.set(K_WORKFLOW_RUN_UID, Json(workflow_run.run_uid));
    tracing::Span::current().set_attribute(
        "moa.experiment.workflow_run_uid",
        workflow_run.run_uid.to_string(),
    );

    workflow_status_response(
        ctx,
        ExperimentRunStatusRequest {
            workspace_id: request.workspace_id,
            run_uid: request.run_uid,
        },
    )
    .await
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    model: ModelId,
    request: &ExperimentRunWorkflowRequest,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = OrchestratorCtx::current().session_store.clone();
    let identity = request.identity.clone();
    Ok(ctx
        .run(|| async move {
            let meta = new_session_meta(workspace_id, model, &identity)?;
            let session_id = create_session_for_identity(store.as_ref(), meta.clone(), identity)
                .await
                .map_err(non_retryable_handler_error)?;
            Ok::<_, HandlerError>(Json::from((session_id, meta)))
        })
        .name("experiment_create_session")
        .await?
        .into_inner())
}

async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    scope: MemoryScope,
    experiment_run_uid: Uuid,
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
            attach_workflow_run(pool, scope, experiment_run_uid, run.run_uid).await?;
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
            }))
        })
        .name("experiment_start_workflow_run")
        .await?
        .into_inner())
}
