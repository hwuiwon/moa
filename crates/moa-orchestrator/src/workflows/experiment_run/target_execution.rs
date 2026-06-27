//! Target execution paths for behavior-lab experiment runs.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartedWorkflowRun {
    run_uid: Uuid,
}

#[derive(Debug, Clone)]
struct WorkflowTargetStart {
    scope: ActionRuleScope,
    experiment_run_uid: Uuid,
    workflow_ref: String,
    input: Value,
    session_id: Option<SessionId>,
    idempotency_key: Option<String>,
    tenant_id: TenantId,
    identity: Identity,
}

pub(super) async fn run_agent_loop_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
    prompt: String,
    session_id: Option<SessionId>,
    agent: Option<AgentSessionSelection>,
    model: ModelId,
    attachments: Vec<moa_core::Attachment>,
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
    )
    .await?;

    let model = variant.model.unwrap_or(model);
    let session_id = match session_id {
        Some(session_id) => session_id,
        None => {
            let agent = agent.ok_or_else(|| {
                bad_request("agent-loop experiment target requires an agent selector")
            })?;
            let (session_id, meta) =
                create_new_session(ctx, request.tenant_id, model.clone(), &request, agent).await?;
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
    persist_attached_session(ctx, scope, request.run_uid, session_id).await?;

    with_identity_headers(
        ctx.object_client::<SessionClient>(session_id.to_string())
            .queue_message(Json::from(QueueMessageRequest {
                user_message: prompt,
                attachments,
                model: Some(model.to_string()),
                contact: None,
                max_turns: None,
            })),
        &request.identity,
    )
    .call()
    .await?;

    workflow_status_response(
        ctx,
        ExperimentRunStatusRequest {
            tenant_id: request.tenant_id,
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
    let scope = tenant_scope(request.tenant_id);
    persist_run_status(
        ctx,
        request.tenant_id,
        request.run_uid,
        ExperimentRunStatus::Running,
        None,
        None,
    )
    .await?;

    let workflow_run = start_and_attach_workflow_run(
        ctx,
        WorkflowTargetStart {
            scope,
            experiment_run_uid: request.run_uid,
            workflow_ref,
            input,
            session_id,
            idempotency_key,
            tenant_id: request.tenant_id,
            identity: request.identity.clone(),
        },
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
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
    )
    .await
}

async fn create_new_session(
    ctx: &WorkflowContext<'_>,
    tenant_id: TenantId,
    model: ModelId,
    request: &ExperimentRunWorkflowRequest,
    agent: AgentSessionSelection,
) -> Result<(SessionId, SessionMeta), HandlerError> {
    let store = OrchestratorCtx::current().session_store_backend();
    let pool = OrchestratorCtx::current_graph_pool();
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

async fn start_and_attach_workflow_run(
    ctx: &WorkflowContext<'_>,
    start: WorkflowTargetStart,
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
            attach_workflow_run(pool, start.scope, start.experiment_run_uid, run.run_uid).await?;
            Ok::<_, HandlerError>(Json::from(StartedWorkflowRun {
                run_uid: run.run_uid,
            }))
        })
        .name("experiment_start_workflow_run")
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
