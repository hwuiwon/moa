//! Restate service for authorized live behavior experiment run metadata.

use chrono::Utc;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::{
    EvalCompareRequest, EvalScoresRequest, ExperimentCancelRequest, ExperimentCancelResponse,
    ExperimentCompareRequest, ExperimentCompareResponse, ExperimentListRequest,
    ExperimentListResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentRunStatusRequest, ExperimentRunStatusResponse, ExperimentScoresRequest,
    ExperimentScoresResponse,
};
use moa_core::{MemoryScope, MoaError, WorkspaceId};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentTarget,
    ExperimentVariant, NewExperimentRun,
};
use moa_experiments::store::ExperimentStore;
use restate_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::eval::EvalServiceError;
use crate::services::score_queries::{
    compare_score_runs_for_workspace, score_summaries_for_workspace,
};
use crate::workflows::experiment_run::{ExperimentRunClient, ExperimentRunWorkflowRequest};

const DEFAULT_LIST_LIMIT: i64 = 50;

/// Restate service surface for live behavior experiment runs.
#[restate_sdk::service]
#[name = "Experiments"]
pub trait Experiments {
    /// Accepts and stores a live behavior experiment run after a workspace editor check.
    async fn run(
        request: Json<ExperimentRunRequest>,
    ) -> Result<Json<ExperimentRunResponse>, HandlerError>;

    /// Loads one live behavior experiment run status after a workspace member check.
    async fn status(
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;

    /// Lists live behavior experiment runs after a workspace member check.
    async fn list(
        request: Json<ExperimentListRequest>,
    ) -> Result<Json<ExperimentListResponse>, HandlerError>;

    /// Cancels a live behavior experiment run after a workspace editor check.
    async fn cancel(
        request: Json<ExperimentCancelRequest>,
    ) -> Result<Json<ExperimentCancelResponse>, HandlerError>;

    /// Reads score summaries for an experiment run after a workspace member check.
    async fn scores(
        request: Json<ExperimentScoresRequest>,
    ) -> Result<Json<ExperimentScoresResponse>, HandlerError>;

    /// Compares score summaries for two experiment runs after a workspace member check.
    async fn compare(
        request: Json<ExperimentCompareRequest>,
    ) -> Result<Json<ExperimentCompareResponse>, HandlerError>;
}

/// Concrete live behavior experiment service implementation.
#[derive(Clone, Default)]
pub struct ExperimentsImpl;

impl Experiments for ExperimentsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentRunRequest>,
    ) -> Result<Json<ExperimentRunResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "run");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        let accepted = ctx
            .run(|| async move { run_inner(pool, request, identity).await.map(Json::from) })
            .name("experiments_run")
            .await?
            .into_inner();
        ctx.workflow_client::<ExperimentRunClient>(accepted.workflow_request.run_uid.to_string())
            .run(Json::from(accepted.workflow_request))
            .send();
        Ok(Json::from(accepted.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "status");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        ctx.workflow_client::<ExperimentRunClient>(request.run_uid.to_string())
            .status(Json::from(request))
            .call()
            .await
            .map_err(HandlerError::from)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentListRequest>,
    ) -> Result<Json<ExperimentListResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "list");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { list_inner(pool, request).await.map(Json::from) })
            .name("experiments_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentCancelRequest>,
    ) -> Result<Json<ExperimentCancelResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "cancel");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { cancel_inner(pool, request).await.map(Json::from) })
            .name("experiments_cancel")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn scores(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentScoresRequest>,
    ) -> Result<Json<ExperimentScoresResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "scores");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { scores_inner(pool, request).await.map(Json::from) })
            .name("experiments_scores")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn compare(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentCompareRequest>,
    ) -> Result<Json<ExperimentCompareResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "compare");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { compare_inner(pool, request).await.map(Json::from) })
            .name("experiments_compare")
            .await?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcceptedExperimentRun {
    response: ExperimentRunResponse,
    workflow_request: ExperimentRunWorkflowRequest,
}

async fn run_inner(
    pool: sqlx::PgPool,
    request: ExperimentRunRequest,
    identity: Identity,
) -> Result<AcceptedExperimentRun, HandlerError> {
    let target = parse_payload::<ExperimentTarget>("target", request.target)?;
    let variant = parse_payload::<ExperimentVariant>("variant", request.variant)?;
    let scorecard = parse_payload::<ExperimentScorecard>("scorecard", request.scorecard)?;
    let score_run_id = request.score_run_id.unwrap_or_else(Uuid::now_v7);
    let scope = workspace_scope(request.workspace_id.clone());
    let run = ExperimentStore::new(pool)
        .insert_run(
            &scope,
            NewExperimentRun {
                name: request.name,
                session_id: session_id_from_target(&target),
                workflow_run_uid: None,
                artifact_revision_uids: variant.artifact_revision_uids.clone(),
                score_run_id,
                target,
                variant,
                scorecard,
                idempotency_key: request.idempotency_key,
                created_by_identity: identity_payload(identity.clone())?,
            },
        )
        .await
        .map_err(moa_error_to_handler_error)?;

    let workflow_request = ExperimentRunWorkflowRequest {
        workspace_id: request.workspace_id.clone(),
        run_uid: run.run_uid,
        target: serialized_payload("target", &run.target)?,
        variant: serialized_payload("variant", &run.variant)?,
        identity,
        score_run_id: run.score_run_id,
    };

    Ok(AcceptedExperimentRun {
        response: run_response_from_record(request.workspace_id, &run),
        workflow_request,
    })
}

async fn list_inner(
    pool: sqlx::PgPool,
    request: ExperimentListRequest,
) -> Result<ExperimentListResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let status = request.status.as_deref().map(parse_status).transpose()?;
    let limit = request
        .limit
        .map(|limit| i64::try_from(limit).map_err(|_| bad_request("limit is too large")))
        .transpose()?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let runs = ExperimentStore::new(pool)
        .list_runs(&scope, status, limit)
        .await
        .map_err(moa_error_to_handler_error)?;
    let runs = runs
        .into_iter()
        .map(record_value)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ExperimentListResponse {
        workspace_id: request.workspace_id,
        runs,
    })
}

async fn cancel_inner(
    pool: sqlx::PgPool,
    request: ExperimentCancelRequest,
) -> Result<ExperimentCancelResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let reason = request
        .reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "cancelled".to_string());
    let run = ExperimentStore::new(pool)
        .update_run_status(
            &scope,
            request.run_uid,
            ExperimentRunStatus::Cancelled,
            Some(reason.clone()),
            Some(Utc::now()),
        )
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(request.run_uid))?;

    Ok(ExperimentCancelResponse {
        workspace_id: request.workspace_id,
        run_uid: run.run_uid,
        cancelled: true,
        status: run.status.as_str().to_string(),
        reason,
    })
}

async fn scores_inner(
    pool: sqlx::PgPool,
    request: ExperimentScoresRequest,
) -> Result<ExperimentScoresResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let run =
        load_required_run(&ExperimentStore::new(pool.clone()), &scope, request.run_uid).await?;
    let score_response = score_summaries_for_workspace(
        &pool,
        EvalScoresRequest {
            workspace_id: request.workspace_id.clone(),
            run_id: run.score_run_id,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;

    Ok(ExperimentScoresResponse {
        workspace_id: request.workspace_id,
        run_uid: run.run_uid,
        score_run_id: run.score_run_id,
        rows: score_response
            .rows
            .into_iter()
            .map(row_value)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

async fn compare_inner(
    pool: sqlx::PgPool,
    request: ExperimentCompareRequest,
) -> Result<ExperimentCompareResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let store = ExperimentStore::new(pool.clone());
    let base_run = load_required_run(&store, &scope, request.base_run_uid).await?;
    let new_run = load_required_run(&store, &scope, request.new_run_uid).await?;
    let compare_response = compare_score_runs_for_workspace(
        &pool,
        EvalCompareRequest {
            workspace_id: request.workspace_id.clone(),
            base_run: base_run.score_run_id,
            new_run: new_run.score_run_id,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;

    Ok(ExperimentCompareResponse {
        workspace_id: request.workspace_id,
        base_run_uid: base_run.run_uid,
        new_run_uid: new_run.run_uid,
        base_score_run_id: base_run.score_run_id,
        new_score_run_id: new_run.score_run_id,
        rows: compare_response
            .rows
            .into_iter()
            .map(row_value)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

async fn load_required_run(
    store: &ExperimentStore,
    scope: &MemoryScope,
    run_uid: Uuid,
) -> Result<ExperimentRunRecord, HandlerError> {
    store
        .load_run(scope, run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| run_not_found(run_uid))
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

fn workspace_scope(workspace_id: WorkspaceId) -> MemoryScope {
    MemoryScope::Workspace { workspace_id }
}

fn parse_payload<T>(field: &'static str, value: Value) -> Result<T, HandlerError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid experiment {field}: {error}")).into()
    })
}

fn serialized_payload<T>(field: &'static str, value: &T) -> Result<Value, HandlerError>
where
    T: Serialize,
{
    serde_json::to_value(value).map_err(|error| {
        TerminalError::new(format!("serialize experiment {field} failed: {error}")).into()
    })
}

fn parse_status(status: &str) -> Result<ExperimentRunStatus, HandlerError> {
    ExperimentRunStatus::from_db(status)
        .ok_or_else(|| bad_request(format!("invalid experiment status `{status}`")))
}

fn session_id_from_target(target: &ExperimentTarget) -> Option<moa_core::SessionId> {
    match target {
        ExperimentTarget::AgentLoop { session_id, .. }
        | ExperimentTarget::Workflow { session_id, .. } => *session_id,
    }
}

fn identity_payload(identity: Identity) -> Result<Value, HandlerError> {
    serde_json::to_value(identity)
        .map_err(|error| TerminalError::new(format!("serialize identity failed: {error}")).into())
}

fn run_response_from_record(
    workspace_id: WorkspaceId,
    run: &ExperimentRunRecord,
) -> ExperimentRunResponse {
    ExperimentRunResponse {
        workspace_id,
        run_uid: run.run_uid,
        status: run.status.as_str().to_string(),
        score_run_id: run.score_run_id,
        session_id: run.session_id,
        workflow_run_uid: run.workflow_run_uid,
    }
}

fn record_value(run: ExperimentRunRecord) -> Result<Value, HandlerError> {
    serde_json::to_value(run).map_err(|error| {
        TerminalError::new(format!("serialize experiment run failed: {error}")).into()
    })
}

fn row_value<T: serde::Serialize>(row: T) -> Result<Value, HandlerError> {
    serde_json::to_value(row)
        .map_err(|error| TerminalError::new(format!("serialize score row failed: {error}")).into())
}

fn run_not_found(run_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment run {run_uid} not found")).into()
}

fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

fn score_error_to_handler_error(error: EvalServiceError) -> HandlerError {
    match error {
        EvalServiceError::IntegerTooLarge { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        EvalServiceError::InvalidDocument { .. }
        | EvalServiceError::DatasetWorkspaceMismatch { .. }
        | EvalServiceError::EmptyWorkspaceDataset { .. }
        | EvalServiceError::RunWorkspaceMismatch { .. }
        | EvalServiceError::Json(_)
        | EvalServiceError::Eval(_)
        | EvalServiceError::Sql(_)
        | EvalServiceError::Lineage(_)
        | EvalServiceError::Runtime { .. } => TerminalError::new(error.to_string()).into(),
    }
}
