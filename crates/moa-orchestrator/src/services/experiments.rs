//! Restate service for authorized live behavior experiment run metadata.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse,
    ExperimentListRequest, ExperimentListResponse, ExperimentProposeImprovementsRequest,
    ExperimentProposeImprovementsResponse, ExperimentRunRequest, ExperimentRunResponse,
    ExperimentRunStatusRequest, ExperimentRunStatusResponse, ExperimentScoresRequest,
    ExperimentScoresResponse, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse,
    ExperimentTrialsRequest, ExperimentTrialsResponse,
};
use moa_core::{MoaError, WorkspaceId, record_experiment_learning_candidates};
use moa_experiments::app::{
    ExperimentAppError, admit_run, cancel_run, compare_runs, list_runs, list_trials,
    plan_generation_request, propose_improvement_candidate, scores, store_generated_plan,
    trial_status,
};
use moa_scoring::ScoringError;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use std::sync::Arc;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::llm_gateway::LLMGatewayImpl;
use crate::workflows::experiment_run::{ExperimentRunClient, ExperimentRunWorkflowRequest};

/// Restate service surface for live behavior experiment runs.
#[restate_sdk::service]
#[name = "Experiments"]
pub trait Experiments {
    /// Generates and stores a draft experiment plan artifact after a workspace editor check.
    async fn generate_plan(
        request: Json<ExperimentGeneratePlanRequest>,
    ) -> Result<Json<ExperimentGeneratePlanResponse>, HandlerError>;

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

    /// Lists live behavior experiment trials after a workspace member check.
    async fn trials(
        request: Json<ExperimentTrialsRequest>,
    ) -> Result<Json<ExperimentTrialsResponse>, HandlerError>;

    /// Loads one live behavior experiment trial status after a workspace member check.
    async fn trial_status(
        request: Json<ExperimentTrialStatusRequest>,
    ) -> Result<Json<ExperimentTrialStatusResponse>, HandlerError>;

    /// Cancels a live behavior experiment run after a workspace editor check.
    async fn cancel(
        request: Json<ExperimentCancelRequest>,
    ) -> Result<Json<ExperimentCancelResponse>, HandlerError>;

    /// Proposes human-reviewed learning candidates from completed experiment evidence.
    async fn propose_improvements(
        request: Json<ExperimentProposeImprovementsRequest>,
    ) -> Result<Json<ExperimentProposeImprovementsResponse>, HandlerError>;

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
    async fn generate_plan(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentGeneratePlanRequest>,
    ) -> Result<Json<ExperimentGeneratePlanResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "generate_plan");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let runtime = OrchestratorCtx::current();
        let pool = runtime.graph_pool();
        let gateway = LLMGatewayImpl::new(runtime.provider_registry());

        Ok(ctx
            .run(|| async move {
                generate_plan_inner(pool, gateway, request)
                    .await
                    .map(Json::from)
            })
            .name("experiments_generate_plan")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentRunRequest>,
    ) -> Result<Json<ExperimentRunResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "run");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        let accepted = ctx
            .run(|| async move { run_inner(pool, request, identity).await.map(Json::from) })
            .name("experiments_run")
            .await?
            .into_inner();
        let workflow_request = ExperimentRunWorkflowRequest {
            workspace_id: accepted.response.workspace_id.clone(),
            run_uid: accepted.run_uid,
            target: accepted.target,
            variant: accepted.variant,
            plan_revision_uid: accepted.plan_revision_uid,
            identity: accepted.identity,
            score_run_id: accepted.score_run_id,
        };
        ctx.workflow_client::<ExperimentRunClient>(workflow_request.run_uid.to_string())
            .run(Json::from(workflow_request))
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { list_inner(pool, request).await.map(Json::from) })
            .name("experiments_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn trials(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentTrialsRequest>,
    ) -> Result<Json<ExperimentTrialsResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "trials");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { trials_inner(pool, request).await.map(Json::from) })
            .name("experiments_trials")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn trial_status(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentTrialStatusRequest>,
    ) -> Result<Json<ExperimentTrialStatusResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "trial_status");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { trial_status_inner(pool, request).await.map(Json::from) })
            .name("experiments_trial_status")
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { cancel_inner(pool, request).await.map(Json::from) })
            .name("experiments_cancel")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn propose_improvements(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentProposeImprovementsRequest>,
    ) -> Result<Json<ExperimentProposeImprovementsResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "propose_improvements");
        let request = request.into_inner();
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let tenant_id = identity.tenant_id.to_string();
        let runtime = OrchestratorCtx::current();
        let pool = runtime.graph_pool();
        let session_store = runtime.session_store();

        Ok(ctx
            .run(|| async move {
                propose_improvements_inner(pool, session_store, request, tenant_id)
                    .await
                    .map(Json::from)
            })
            .name("experiments_propose_improvements")
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
        let pool = OrchestratorCtx::current_graph_pool();

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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { compare_inner(pool, request).await.map(Json::from) })
            .name("experiments_compare")
            .await?)
    }
}

async fn generate_plan_inner(
    pool: sqlx::PgPool,
    gateway: LLMGatewayImpl,
    request: ExperimentGeneratePlanRequest,
) -> Result<ExperimentGeneratePlanResponse, HandlerError> {
    let completion = gateway
        .complete_buffered(
            plan_generation_request(&request).map_err(experiment_app_error_to_handler_error)?,
        )
        .await
        .map_err(moa_error_to_handler_error)?;

    store_generated_plan(pool, request, &completion.text)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn run_inner(
    pool: sqlx::PgPool,
    request: ExperimentRunRequest,
    identity: Identity,
) -> Result<moa_experiments::app::AdmittedExperimentRun, HandlerError> {
    admit_run(pool, request, identity)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn list_inner(
    pool: sqlx::PgPool,
    request: ExperimentListRequest,
) -> Result<ExperimentListResponse, HandlerError> {
    list_runs(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn trials_inner(
    pool: sqlx::PgPool,
    request: ExperimentTrialsRequest,
) -> Result<ExperimentTrialsResponse, HandlerError> {
    list_trials(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn trial_status_inner(
    pool: sqlx::PgPool,
    request: ExperimentTrialStatusRequest,
) -> Result<ExperimentTrialStatusResponse, HandlerError> {
    trial_status(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn cancel_inner(
    pool: sqlx::PgPool,
    request: ExperimentCancelRequest,
) -> Result<ExperimentCancelResponse, HandlerError> {
    cancel_run(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn propose_improvements_inner(
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
    request: ExperimentProposeImprovementsRequest,
    tenant_id: String,
) -> Result<ExperimentProposeImprovementsResponse, HandlerError> {
    let proposal = propose_improvement_candidate(pool, request, tenant_id)
        .await
        .map_err(experiment_app_error_to_handler_error)?;
    session_store
        .append_learning_candidate(&proposal.candidate)
        .await
        .map_err(moa_error_to_handler_error)?;
    record_experiment_learning_candidates(proposal.candidate.status.as_str(), 1);

    Ok(proposal.response)
}

async fn scores_inner(
    pool: sqlx::PgPool,
    request: ExperimentScoresRequest,
) -> Result<ExperimentScoresResponse, HandlerError> {
    scores(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn compare_inner(
    pool: sqlx::PgPool,
    request: ExperimentCompareRequest,
) -> Result<ExperimentCompareResponse, HandlerError> {
    compare_runs(pool, request)
        .await
        .map_err(experiment_app_error_to_handler_error)
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

fn experiment_app_error_to_handler_error(error: ExperimentAppError) -> HandlerError {
    match error {
        ExperimentAppError::BadRequest(message) => {
            TerminalError::new_with_code(400, message).into()
        }
        ExperimentAppError::NotFound(message) => TerminalError::new_with_code(404, message).into(),
        ExperimentAppError::Serialization(message) => TerminalError::new(message).into(),
        ExperimentAppError::Moa(error) => moa_error_to_handler_error(error),
        ExperimentAppError::Scoring(error) => score_error_to_handler_error(error),
    }
}

fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}

fn score_error_to_handler_error(error: ScoringError) -> HandlerError {
    match error {
        ScoringError::IntegerTooLarge { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        ScoringError::Sql(_) | ScoringError::ScoreRunMismatch { .. } => {
            TerminalError::new(error.to_string()).into()
        }
    }
}
