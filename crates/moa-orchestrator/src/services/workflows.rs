//! Restate service adapter for artifact-backed workflow run lifecycle operations.

use moa_artifacts::registry::ArtifactRegistry;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::ActionRuleScope;
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::wire::{
    WorkflowCancelRequest, WorkflowCancelResponse, WorkflowNodeRunSummary,
    WorkflowReviewDecisionRequest, WorkflowReviewDecisionResponse, WorkflowRunRequest,
    WorkflowRunResponse, WorkflowRunStatus, WorkflowSignalRequest, WorkflowSignalResponse,
    WorkflowStatusRequest,
};
use moa_workflows::error::WorkflowError;
use moa_workflows::runtime::{StartWorkflowRun, WorkflowRuntime};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::workflows::artifact_workflow_execution::{
    ArtifactWorkflowExecutionClient, RunArtifactWorkflowRequest, validate_workflow_review_decision,
    validate_workflow_signal,
};
use crate::workflows::errors::workflow_handler_error;

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

    /// Decides a pending workflow review node.
    async fn decide_review(
        request: Json<WorkflowReviewDecisionRequest>,
    ) -> Result<Json<WorkflowReviewDecisionResponse>, HandlerError>;

    /// Resolves a pending workflow wait-signal node.
    async fn signal(
        request: Json<WorkflowSignalRequest>,
    ) -> Result<Json<WorkflowSignalResponse>, HandlerError>;
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
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let execution_request_tenant_id = request.tenant_id;
        let execution_request_session_id = request.session_id;

        let response = ctx
            .run(|| async move { run_inner(request).await.map(Json::from) })
            .name("workflows_run")
            .await?
            .into_inner();
        ctx.workflow_client::<ArtifactWorkflowExecutionClient>(response.run_id.to_string())
            .run(Json::from(RunArtifactWorkflowRequest {
                tenant_id: execution_request_tenant_id,
                run_uid: response.run_id,
                identity,
                session_id: execution_request_session_id,
            }))
            .send();
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowStatusRequest>,
    ) -> Result<Json<WorkflowRunStatus>, HandlerError> {
        annotate_restate_handler_span("Workflows", "status");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

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
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let run_uid = request.run_id;
        let reason = request
            .reason
            .clone()
            .unwrap_or_else(|| "workflow cancellation requested".to_string());

        let response = ctx
            .run(|| async move { cancel_inner(request).await.map(Json::from) })
            .name("workflows_cancel")
            .await?
            .into_inner();
        if response.cancelled {
            ctx.workflow_client::<ArtifactWorkflowExecutionClient>(run_uid.to_string())
                .request_cancel(Json::from(reason))
                .send();
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide_review(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowReviewDecisionRequest>,
    ) -> Result<Json<WorkflowReviewDecisionResponse>, HandlerError> {
        annotate_restate_handler_span("Workflows", "decide_review");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let run_uid = request.run_id;

        let validated = ctx
            .run(|| async move {
                validate_workflow_review_decision(request)
                    .await
                    .map(Json::from)
            })
            .name("workflows_decide_review")
            .await?
            .into_inner();
        if let Some(resolution) = validated.resolution {
            return ctx
                .workflow_client::<ArtifactWorkflowExecutionClient>(run_uid.to_string())
                .decide_review(Json::from(resolution))
                .call()
                .await
                .map_err(HandlerError::from);
        }
        Ok(Json::from(validated.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn signal(
        &self,
        ctx: Context<'_>,
        request: Json<WorkflowSignalRequest>,
    ) -> Result<Json<WorkflowSignalResponse>, HandlerError> {
        annotate_restate_handler_span("Workflows", "signal");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let run_uid = request.run_id;

        let validated = ctx
            .run(|| async move { validate_workflow_signal(request).await.map(Json::from) })
            .name("workflows_signal")
            .await?
            .into_inner();
        if let Some(resolution) = validated.resolution {
            return ctx
                .workflow_client::<ArtifactWorkflowExecutionClient>(run_uid.to_string())
                .signal(Json::from(resolution))
                .call()
                .await
                .map_err(HandlerError::from);
        }
        Ok(Json::from(validated.response))
    }
}

async fn run_inner(request: WorkflowRunRequest) -> Result<WorkflowRunResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
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
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = workflow_runtime()
        .status(&scope, request.run_id)
        .await
        .map_err(workflow_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "workflow run not found"))?;
    let node_runs = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool())
        .list_node_runs(&scope, request.run_id)
        .await
        .map_err(|error| workflow_handler_error(WorkflowError::Artifact(error)))?
        .into_iter()
        .map(|node_run| WorkflowNodeRunSummary {
            node_id: node_run.node_id,
            status: node_run.status.as_str().to_string(),
            started_at: node_run.started_at,
            completed_at: node_run.completed_at,
        })
        .collect();
    Ok(WorkflowRunStatus {
        run_id: run.run_uid,
        session_id: run.session_id,
        current_node_id: run.current_node_id,
        status: run.status.as_str().to_string(),
        node_runs,
        output: run.output,
        error: run.error,
    })
}

async fn cancel_inner(
    request: WorkflowCancelRequest,
) -> Result<WorkflowCancelResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
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
    WorkflowRuntime::new(ArtifactRegistry::new(OrchestratorCtx::current_graph_pool()))
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: moa_core::TenantId,
    relation: Relation,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .map_err(translate_authz_error)?;
    Ok(identity)
}
