//! Restate workflow that admits one live behavior experiment into production paths.

use std::time::Duration;

use chrono::Utc;
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRunStatus};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    ExperimentRunStatusRequest, ExperimentRunStatusResponse, QueueMessageRequest,
};
use moa_core::{
    MemoryScope, MoaError, ModelId, Platform, SessionId, SessionMeta, SessionStatus, SessionStore,
    UserId, WorkspaceId,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentTarget, ExperimentTargetKind,
    ExperimentVariant,
};
use moa_experiments::store::ExperimentStore;
use moa_workflows::error::WorkflowError;
use moa_workflows::runtime::{StartWorkflowRun, WorkflowRuntime};
use restate_sdk::context::Request;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::objects::session::SessionClient;
use crate::services::session_store::inner::create_session_for_identity;

const K_WORKSPACE_ID: &str = "workspace_id";
const K_RUN_UID: &str = "run_uid";
const K_SCORE_RUN_ID: &str = "score_run_id";
const K_STATUS: &str = "status";
const K_SESSION_ID: &str = "session_id";
const K_WORKFLOW_RUN_UID: &str = "workflow_run_uid";

/// Workflow input for one live behavior experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunWorkflowRequest {
    /// Workspace already authorized by `Experiments/run`.
    pub workspace_id: WorkspaceId,
    /// Durable experiment run identifier.
    pub run_uid: Uuid,
    /// Serialized experiment target payload.
    pub target: Value,
    /// Serialized experiment variant payload.
    pub variant: Value,
    /// Identity snapshot used for audit and normal downstream authz checks.
    pub identity: Identity,
    /// Score run identifier associated with this experiment.
    pub score_run_id: Uuid,
}

/// Restate workflow surface for one live behavior experiment run.
#[restate_sdk::workflow]
pub trait ExperimentRun {
    /// Drives an accepted experiment run into the matching production path.
    async fn run(
        request: Json<ExperimentRunWorkflowRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;

    /// Reads the current experiment status after service-layer authorization.
    #[shared]
    async fn status(
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;
}

/// Concrete live behavior experiment workflow implementation.
pub struct ExperimentRunImpl;

impl ExperimentRun for ExperimentRunImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Experiments/run after workspace editor authz.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<ExperimentRunWorkflowRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("ExperimentRun", "run");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }

        ctx.set(K_WORKSPACE_ID, Json(request.workspace_id.clone()));
        ctx.set(K_RUN_UID, Json(request.run_uid));
        ctx.set(K_SCORE_RUN_ID, Json(request.score_run_id));
        ctx.set(K_STATUS, Json(ExperimentRunStatus::Running));

        match run_experiment_target(&ctx, request.clone()).await {
            Ok(response) => Ok(Json(response)),
            Err(error) => {
                let message = handler_error_message(&error);
                ctx.set(K_STATUS, Json(ExperimentRunStatus::Failed));
                let failed_at = durable_utc_now(&ctx).await?;
                if let Err(update_error) = persist_run_status(
                    &ctx,
                    request.workspace_id,
                    request.run_uid,
                    ExperimentRunStatus::Failed,
                    Some(message),
                    Some(failed_at),
                )
                .await
                {
                    tracing::warn!(
                        error = ?update_error,
                        run_uid = %request.run_uid,
                        "failed to persist experiment workflow failure"
                    );
                }
                Err(error)
            }
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: called only from Experiments/status after workspace member authz.
    async fn status(
        &self,
        ctx: SharedWorkflowContext<'_>,
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("ExperimentRun", "status");
        let request = request.into_inner();
        if request.run_uid.to_string() != ctx.key() {
            return Err(TerminalError::new_with_code(404, "experiment run id mismatch").into());
        }
        let pool = OrchestratorCtx::current().graph_pool.clone();
        Ok(ctx
            .run(|| async move { status_response(pool, request).await.map(Json::from) })
            .name("experiment_run_status")
            .await?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StartedWorkflowRun {
    run_uid: Uuid,
}

async fn run_experiment_target(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunWorkflowRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    match parse_payload::<ExperimentTarget>("target", request.target.clone())? {
        ExperimentTarget::AgentLoop {
            prompt,
            session_id,
            model,
            attachments,
        } => run_agent_loop_target(ctx, request, prompt, session_id, model, attachments).await,
        ExperimentTarget::Workflow {
            workflow_ref,
            input,
            session_id,
            idempotency_key,
        } => {
            run_workflow_target(
                ctx,
                request,
                workflow_ref,
                input,
                session_id,
                idempotency_key,
            )
            .await
        }
    }
}

async fn run_agent_loop_target(
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

async fn run_workflow_target(
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

async fn status_response(
    pool: sqlx::PgPool,
    request: ExperimentRunStatusRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let workspace_id = request.workspace_id.clone();
    let scope = workspace_scope(workspace_id.clone());
    let store = ExperimentStore::new(pool.clone());
    let mut run = store
        .load_run(&scope, request.run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(request.run_uid))?;

    if run.target_kind == ExperimentTargetKind::Workflow {
        return linked_workflow_status_response(pool, scope, workspace_id, run).await;
    }

    if let Some(status) = derived_session_status(run.status, run.session_id).await?
        && status != run.status
    {
        run = store
            .update_run_status(
                &scope,
                run.run_uid,
                status,
                None,
                completed_at_for_status(status),
            )
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| run_not_found(request.run_uid))?;
    }

    status_response_from_record(workspace_id, run)
}

async fn linked_workflow_status_response(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    workspace_id: WorkspaceId,
    mut run: ExperimentRunRecord,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let Some(workflow_run_uid) = run.workflow_run_uid else {
        return status_response_from_record(workspace_id, run);
    };

    let workflow_run = workflow_runtime(pool.clone())
        .status(&scope, workflow_run_uid)
        .await
        .map_err(workflow_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;

    if let Some(status) = experiment_status_from_artifact_status(&workflow_run.status)
        && status != run.status
    {
        let run_uid = run.run_uid;
        run = ExperimentStore::new(pool)
            .update_run_status(
                &scope,
                run_uid,
                status,
                workflow_run.error.clone(),
                workflow_run.completed_at,
            )
            .await
            .map_err(moa_error_to_handler_error)?
            .ok_or_else(|| run_not_found(run_uid))?;
    }

    let mut response = status_response_from_record_with_status(
        workspace_id,
        run,
        workflow_run.status.as_str().to_string(),
    )?;
    response.session_id = workflow_run.session_id.or(response.session_id);
    if workflow_run.error.is_some() {
        response.error = workflow_run.error;
    }
    Ok(response)
}

async fn workflow_status_response(
    ctx: &WorkflowContext<'_>,
    request: ExperimentRunStatusRequest,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    Ok(ctx
        .run(|| async move { status_response(pool, request).await.map(Json::from) })
        .name("experiment_run_response")
        .await?
        .into_inner())
}

async fn derived_session_status(
    row_status: ExperimentRunStatus,
    session_id: Option<SessionId>,
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
    let session = OrchestratorCtx::current()
        .session_store
        .get_session(session_id)
        .await
        .map_err(moa_error_to_handler_error)?;
    Ok(Some(match session.status {
        SessionStatus::Created => row_status,
        SessionStatus::Running => ExperimentRunStatus::Running,
        SessionStatus::Paused | SessionStatus::Completed => ExperimentRunStatus::Completed,
        SessionStatus::WaitingApproval => ExperimentRunStatus::WaitingApproval,
        SessionStatus::Cancelled => ExperimentRunStatus::Cancelled,
        SessionStatus::Failed => ExperimentRunStatus::Failed,
    }))
}

async fn persist_run_status(
    ctx: &WorkflowContext<'_>,
    workspace_id: WorkspaceId,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    completed_at: Option<chrono::DateTime<Utc>>,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    let scope = workspace_scope(workspace_id);
    ctx.run(|| async move {
        update_run_status(pool, scope, run_uid, status, error, completed_at).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_update_run_status")
    .await?;
    Ok(())
}

async fn persist_attached_session(
    ctx: &WorkflowContext<'_>,
    scope: MemoryScope,
    run_uid: Uuid,
    session_id: SessionId,
) -> Result<(), HandlerError> {
    let pool = OrchestratorCtx::current().graph_pool.clone();
    ctx.run(|| async move {
        attach_session(pool, scope, run_uid, session_id).await?;
        Ok::<_, HandlerError>(Json::from(()))
    })
    .name("experiment_attach_session")
    .await?;
    Ok(())
}

async fn durable_utc_now(ctx: &WorkflowContext<'_>) -> Result<chrono::DateTime<Utc>, HandlerError> {
    Ok(ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("experiment_utc_now")
        .await?
        .into_inner())
}

async fn update_run_status(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    run_uid: Uuid,
    status: ExperimentRunStatus,
    error: Option<String>,
    completed_at: Option<chrono::DateTime<Utc>>,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .update_run_status(&scope, run_uid, status, error, completed_at)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

async fn attach_session(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    run_uid: Uuid,
    session_id: SessionId,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_session(&scope, run_uid, session_id)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

async fn attach_workflow_run(
    pool: sqlx::PgPool,
    scope: MemoryScope,
    run_uid: Uuid,
    workflow_run_uid: Uuid,
) -> Result<ExperimentRunRecord, HandlerError> {
    ExperimentStore::new(pool)
        .attach_workflow_run(&scope, run_uid, workflow_run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
}

fn new_session_meta(
    workspace_id: WorkspaceId,
    model: ModelId,
    identity: &Identity,
) -> Result<SessionMeta, HandlerError> {
    let now = Utc::now();
    Ok(SessionMeta {
        id: SessionId::new(),
        workspace_id,
        user_id: session_user_id(identity)?,
        title: Some("Experiment agent-loop run".to_string()),
        status: SessionStatus::Created,
        platform: Platform::Api,
        model,
        created_at: now,
        updated_at: now,
        ..SessionMeta::default()
    })
}

fn session_user_id(identity: &Identity) -> Result<UserId, HandlerError> {
    match identity.identity_type {
        IdentityType::User => Ok(UserId::new(identity.id.to_string())),
        IdentityType::Agent => Ok(UserId::new(
            identity
                .acting_on_behalf_of
                .unwrap_or(identity.id)
                .to_string(),
        )),
        IdentityType::Service => Err(TerminalError::new_with_code(
            403,
            "service identities cannot create agent-loop experiment sessions",
        )
        .into()),
    }
}

fn with_identity_headers<'a, Req, Res>(
    request: Request<'a, Req, Res>,
    identity: &Identity,
) -> Request<'a, Req, Res> {
    let request = request
        .header(
            "x-moa-identity-type".to_string(),
            identity_type_header(identity.identity_type).to_string(),
        )
        .header("x-moa-identity-id".to_string(), identity.id.to_string())
        .header(
            "x-moa-tenant-id".to_string(),
            identity.tenant_id.to_string(),
        );
    let request = if let Some(api_key_id) = identity.api_key_id {
        request.header("x-moa-api-key-id".to_string(), api_key_id.to_string())
    } else {
        request
    };
    if let Some(user_id) = identity.acting_on_behalf_of {
        request.header("x-moa-acting-on-behalf-of".to_string(), user_id.to_string())
    } else {
        request
    }
}

fn identity_type_header(identity_type: IdentityType) -> &'static str {
    match identity_type {
        IdentityType::User => "user",
        IdentityType::Agent => "agent",
        IdentityType::Service => "service",
    }
}

fn completed_at_for_status(status: ExperimentRunStatus) -> Option<chrono::DateTime<Utc>> {
    if matches!(
        status,
        ExperimentRunStatus::Completed
            | ExperimentRunStatus::Failed
            | ExperimentRunStatus::Cancelled
    ) {
        Some(Utc::now())
    } else {
        None
    }
}

fn experiment_status_from_artifact_status(
    status: &ArtifactRunStatus,
) -> Option<ExperimentRunStatus> {
    match status {
        ArtifactRunStatus::Queued => None,
        ArtifactRunStatus::Running => Some(ExperimentRunStatus::Running),
        ArtifactRunStatus::WaitingApproval => Some(ExperimentRunStatus::WaitingApproval),
        ArtifactRunStatus::Completed => Some(ExperimentRunStatus::Completed),
        ArtifactRunStatus::Failed => Some(ExperimentRunStatus::Failed),
        ArtifactRunStatus::Cancelled => Some(ExperimentRunStatus::Cancelled),
    }
}

fn workflow_runtime(pool: sqlx::PgPool) -> WorkflowRuntime {
    WorkflowRuntime::new(ArtifactRegistry::new(pool))
}

fn workspace_scope(workspace_id: WorkspaceId) -> MemoryScope {
    MemoryScope::Workspace { workspace_id }
}

fn parse_payload<T>(field: &'static str, value: Value) -> Result<T, HandlerError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid experiment {field}: {error}")).into()
    })
}

fn status_response_from_record(
    workspace_id: WorkspaceId,
    run: ExperimentRunRecord,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let status = run.status.as_str().to_string();
    status_response_from_record_with_status(workspace_id, run, status)
}

fn status_response_from_record_with_status(
    workspace_id: WorkspaceId,
    run: ExperimentRunRecord,
    status: String,
) -> Result<ExperimentRunStatusResponse, HandlerError> {
    let run_value = serde_json::to_value(&run)
        .map_err(|error| TerminalError::new(format!("serialize experiment run failed: {error}")))?;
    Ok(ExperimentRunStatusResponse {
        workspace_id,
        run_uid: run.run_uid,
        status,
        target_kind: Some(run.target_kind.as_str().to_string()),
        score_run_id: Some(run.score_run_id),
        session_id: run.session_id,
        workflow_run_uid: run.workflow_run_uid,
        error: run.error,
        run: run_value,
    })
}

fn run_not_found(run_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment run {run_uid} not found")).into()
}

fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

fn non_retryable_handler_error(error: HandlerError) -> HandlerError {
    TerminalError::new(handler_error_message(&error)).into()
}

fn handler_error_message(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}

fn workflow_handler_error(error: WorkflowError) -> HandlerError {
    match error {
        WorkflowError::InvalidReference { .. } | WorkflowError::WrongReferenceKind => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        WorkflowError::WorkflowNotFound { .. } => {
            TerminalError::new_with_code(404, error.to_string()).into()
        }
        WorkflowError::Artifact(source) => {
            if source.is_fatal() {
                TerminalError::new(source.to_string()).into()
            } else {
                HandlerError::from(source)
            }
        }
    }
}
