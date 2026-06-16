//! Restate service adapter for artifact-backed workflow run lifecycle operations.

use moa_artifacts::registry::ArtifactRegistry;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::{
    WorkflowCancelRequest, WorkflowCancelResponse, WorkflowRunRequest, WorkflowRunResponse,
    WorkflowRunStatus, WorkflowStatusRequest,
};
use moa_core::{MemoryScope, WorkspaceId};
use moa_workflows::error::WorkflowError;
use moa_workflows::runtime::{StartWorkflowRun, WorkflowRuntime};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Restate service surface for workflow run lifecycle operations.
#[restate_sdk::service]
#[name = "Workflows"]
pub trait Workflows {
    /// Creates a durable workflow run row.
    async fn run(
        request: Json<WorkflowRunRequest>,
    ) -> Result<Json<WorkflowRunResponse>, HandlerError>;

    /// Loads workflow run status.
    async fn status(
        request: Json<WorkflowStatusRequest>,
    ) -> Result<Json<WorkflowRunStatus>, HandlerError>;

    /// Requests workflow run cancellation.
    async fn cancel(
        request: Json<WorkflowCancelRequest>,
    ) -> Result<Json<WorkflowCancelResponse>, HandlerError>;
}

/// Concrete workflow service implementation.
#[derive(Clone, Default)]
pub struct WorkflowsImpl;

impl Workflows for WorkflowsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowRunRequest>,
    ) -> Result<Json<WorkflowRunResponse>, HandlerError> {
        annotate_restate_handler_span("Workflows", "run");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;

        Ok(ctx
            .run(|| async move { run_inner(request).await.map(Json::from) })
            .name("workflows_run")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowStatusRequest>,
    ) -> Result<Json<WorkflowRunStatus>, HandlerError> {
        annotate_restate_handler_span("Workflows", "status");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { status_inner(request).await.map(Json::from) })
            .name("workflows_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowCancelRequest>,
    ) -> Result<Json<WorkflowCancelResponse>, HandlerError> {
        annotate_restate_handler_span("Workflows", "cancel");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;

        Ok(ctx
            .run(|| async move { cancel_inner(request).await.map(Json::from) })
            .name("workflows_cancel")
            .await?)
    }
}

async fn run_inner(request: WorkflowRunRequest) -> Result<WorkflowRunResponse, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id,
    };
    let run = workflow_runtime()
        .start(
            &scope,
            StartWorkflowRun {
                workflow_ref: request.workflow_ref,
                input: request.input,
                session_id: request.session_id,
                idempotency_key: request.idempotency_key,
            },
        )
        .await
        .map_err(workflow_handler_error)?;

    Ok(WorkflowRunResponse {
        run_id: run.run_uid,
        status: run.status.as_str().to_string(),
    })
}

async fn status_inner(request: WorkflowStatusRequest) -> Result<WorkflowRunStatus, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id,
    };
    let run = workflow_runtime()
        .status(&scope, request.run_id)
        .await
        .map_err(workflow_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    Ok(WorkflowRunStatus {
        run_id: run.run_uid,
        session_id: run.session_id,
        current_node_id: run.current_node_id,
        status: run.status.as_str().to_string(),
        node_runs: Vec::new(),
        output: run.output,
        error: run.error,
    })
}

async fn cancel_inner(
    request: WorkflowCancelRequest,
) -> Result<WorkflowCancelResponse, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id,
    };
    let run = workflow_runtime()
        .cancel(&scope, request.run_id, request.reason)
        .await
        .map_err(workflow_handler_error)?;
    Ok(WorkflowCancelResponse {
        cancelled: run.is_some(),
        reason: run
            .map(|_| "cancelled".to_string())
            .unwrap_or_else(|| "workflow run was not cancellable".to_string()),
    })
}

fn workflow_runtime() -> WorkflowRuntime {
    WorkflowRuntime::new(ArtifactRegistry::new(
        OrchestratorCtx::current().graph_pool.clone(),
    ))
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        relation,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
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
