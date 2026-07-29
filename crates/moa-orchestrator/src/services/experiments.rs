//! Restate service for authorized live behavior experiment run metadata.

use moa_agents::{AgentResolver, AgentRuntimePolicy};
use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::ArtifactRegistry;
use moa_core::traits::{Identity, LearningCandidateStore};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_experiments::app::{
    ExperimentAppError, admit_run, cancel_run, compare_runs, list_runs, list_trials,
    plan_generation_repair_request, plan_generation_request, propose_improvement_candidate, scores,
    store_generated_plan, trial_status,
};
use moa_experiments::model::{ExperimentRunStatus, ExperimentTrialStatus, ExperimentVariant};
use moa_experiments::scores::{
    ExperimentRunScoreRef, TrialScoreSummary, experiment_score_breakdown_for_tenant,
};
use moa_experiments::store::ExperimentStore;
use moa_observability::record_experiment_learning_candidates;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_providers::ProviderRegistry;
use moa_scoring::Error;
use moa_wire::artifacts::ArtifactSummary as WireArtifactSummary;
use moa_wire::experiments::{
    AgentArtifactDependencyDelta, AgentDependencyChange, AgentRevisionCompareRequest,
    AgentRevisionCompareResponse, AgentRevisionSimulationCompareRequest,
    AgentRevisionSimulationCompareResponse, AgentRevisionSimulationRunRequest,
    AgentRevisionSimulationRunResponse, AgentRevisionSimulationVariant,
    AgentRevisionSimulationVariantResult, AgentToolDependencyDelta, ExperimentCancelRequest,
    ExperimentCancelResponse, ExperimentCompareRequest, ExperimentCompareResponse,
    ExperimentGeneratePlanRequest, ExperimentGeneratePlanResponse, ExperimentListRequest,
    ExperimentListResponse, ExperimentPlanListRequest, ExperimentPlanListResponse,
    ExperimentProposeImprovementsRequest, ExperimentProposeImprovementsResponse,
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, ExperimentScoresRequest, ExperimentScoresResponse,
    ExperimentTrialStatusRequest, ExperimentTrialStatusResponse, ExperimentTrialsRequest,
    ExperimentTrialsResponse, ExperimentVariantScoreDeltaRow,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::handlers::authz_shim::authorize_tenant_operator_or_admin;
use crate::services::llm_gateway::LLMGatewayImpl;
use crate::workflows::errors::moa_error_to_handler_error;
use crate::workflows::experiment_run::{ExperimentRunClient, ExperimentRunWorkflowRequest};
use moa_core::types::experiments::ExperimentCancelSignal;

/// Restate service surface for live behavior experiment runs.
#[restate_sdk::service]
#[name = "Experiments"]
pub trait Experiments {
    /// Generates and stores a draft experiment plan artifact after a tenant operator/admin check.
    async fn generate_plan(
        request: Json<ExperimentGeneratePlanRequest>,
    ) -> Result<Json<ExperimentGeneratePlanResponse>, HandlerError>;

    /// Accepts and stores a live behavior experiment run after a tenant operator/admin check.
    async fn run(
        request: Json<ExperimentRunRequest>,
    ) -> Result<Json<ExperimentRunResponse>, HandlerError>;

    /// Loads one live behavior experiment run status after a tenant operator/admin check.
    async fn status(
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError>;

    /// Lists live behavior experiment runs after a tenant operator/admin check.
    async fn list(
        request: Json<ExperimentListRequest>,
    ) -> Result<Json<ExperimentListResponse>, HandlerError>;

    /// Lists visible behavior-lab plan artifacts after a tenant operator/admin check.
    async fn list_plans(
        request: Json<ExperimentPlanListRequest>,
    ) -> Result<Json<ExperimentPlanListResponse>, HandlerError>;

    /// Lists live behavior experiment trials after a tenant operator/admin check.
    async fn trials(
        request: Json<ExperimentTrialsRequest>,
    ) -> Result<Json<ExperimentTrialsResponse>, HandlerError>;

    /// Loads one live behavior experiment trial status after a tenant operator/admin check.
    async fn trial_status(
        request: Json<ExperimentTrialStatusRequest>,
    ) -> Result<Json<ExperimentTrialStatusResponse>, HandlerError>;

    /// Cancels a live behavior experiment run after a tenant operator/admin check.
    async fn cancel(
        request: Json<ExperimentCancelRequest>,
    ) -> Result<Json<ExperimentCancelResponse>, HandlerError>;

    /// Proposes human-reviewed learning candidates from completed experiment evidence.
    async fn propose_improvements(
        request: Json<ExperimentProposeImprovementsRequest>,
    ) -> Result<Json<ExperimentProposeImprovementsResponse>, HandlerError>;

    /// Reads score summaries for an experiment run after a tenant operator/admin check.
    async fn scores(
        request: Json<ExperimentScoresRequest>,
    ) -> Result<Json<ExperimentScoresResponse>, HandlerError>;

    /// Compares score summaries for two experiment runs after a tenant operator/admin check.
    async fn compare(
        request: Json<ExperimentCompareRequest>,
    ) -> Result<Json<ExperimentCompareResponse>, HandlerError>;

    /// Runs a plan-backed simulation across exact agent revisions.
    async fn run_agent_revision_simulation(
        request: Json<AgentRevisionSimulationRunRequest>,
    ) -> Result<Json<AgentRevisionSimulationRunResponse>, HandlerError>;

    /// Compares agent revision variants inside one simulation run.
    async fn compare_agent_revision_simulation(
        request: Json<AgentRevisionSimulationCompareRequest>,
    ) -> Result<Json<AgentRevisionSimulationCompareResponse>, HandlerError>;

    /// Compares resolved runtime policies for two published agent revisions.
    async fn compare_agent_revisions(
        request: Json<AgentRevisionCompareRequest>,
    ) -> Result<Json<AgentRevisionCompareResponse>, HandlerError>;
}

/// Concrete live behavior experiment service implementation.
#[derive(Clone)]
pub struct ExperimentsImpl {
    pool: sqlx::PgPool,
    providers: Arc<ProviderRegistry>,
    learning_candidate_store: Arc<dyn LearningCandidateStore>,
}

impl ExperimentsImpl {
    /// Creates the experiment service with its persistence and provider dependencies.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        providers: Arc<ProviderRegistry>,
        learning_candidate_store: Arc<dyn LearningCandidateStore>,
    ) -> Self {
        Self {
            pool,
            providers,
            learning_candidate_store,
        }
    }
}

impl Experiments for ExperimentsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn generate_plan(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentGeneratePlanRequest>,
    ) -> Result<Json<ExperimentGeneratePlanResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "generate_plan");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();
        let gateway = LLMGatewayImpl::new(self.providers.clone());

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "run");
        let request = request.into_inner();
        let identity = authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        let accepted = ctx
            .run(|| async move { run_inner(pool, request, identity).await.map(Json::from) })
            .name("experiments_run")
            .await?
            .into_inner();
        let workflow_request = ExperimentRunWorkflowRequest {
            tenant_id: accepted.response.tenant_id,
            run_uid: accepted.run_uid,
            target: accepted.target,
            variant: accepted.variant,
            plan_revision_uid: accepted.plan_revision_uid,
            identity: accepted.identity,
            score_run_id: accepted.score_run_id,
            agent_revision_variants: accepted.agent_revision_variants,
        };
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<ExperimentRunClient>(workflow_request.run_uid.to_string())
                .run(Json::from(workflow_request)),
        )
        .send();
        Ok(Json::from(accepted.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentRunStatusRequest>,
    ) -> Result<Json<ExperimentRunStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "status");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;

        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<ExperimentRunClient>(request.run_uid.to_string())
                .status(Json::from(request)),
        )
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "list");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move { list_inner(pool, request).await.map(Json::from) })
            .name("experiments_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_plans(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentPlanListRequest>,
    ) -> Result<Json<ExperimentPlanListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "list_plans");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move { list_plans_inner(pool, request).await.map(Json::from) })
            .name("experiments_list_plans")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn trials(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentTrialsRequest>,
    ) -> Result<Json<ExperimentTrialsResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "trials");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "trial_status");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "cancel");
        let request = request.into_inner();
        let identity = authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();
        let run_uid = request.run_uid;
        let persist_identity = identity.clone();

        let response = ctx
            .run(|| async move {
                cancel_inner(pool, request, persist_identity)
                    .await
                    .map(Json::from)
            })
            .name("experiments_cancel")
            .await?
            .into_inner();

        // After the projection is cancelled, durably propagate cancellation to
        // the run workflow so it stops live child work (its own target and every
        // active trial). Best-effort one-way; only meaningful once the run is
        // cancelled, so genuinely finished runs are not signaled.
        if response.status == ExperimentRunStatus::Cancelled.as_str() {
            crate::restate_identity::replay_safe_request(
                ctx.workflow_client::<ExperimentRunClient>(run_uid.to_string())
                    .request_cancel(Json::from(ExperimentCancelSignal {
                        reason: response.reason.clone(),
                        identity,
                    })),
            )
            .send();
        }

        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn propose_improvements(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentProposeImprovementsRequest>,
    ) -> Result<Json<ExperimentProposeImprovementsResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "propose_improvements");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();
        let session_store = self.learning_candidate_store.clone();

        Ok(ctx
            .run(|| async move {
                propose_improvements_inner(pool, session_store, request)
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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "scores");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

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
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "compare");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move { compare_inner(pool, request).await.map(Json::from) })
            .name("experiments_compare")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run_agent_revision_simulation(
        &self,
        ctx: Context<'_>,
        request: Json<AgentRevisionSimulationRunRequest>,
    ) -> Result<Json<AgentRevisionSimulationRunResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "run_agent_revision_simulation");
        let request = request.into_inner();
        let identity = authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        let accepted = ctx
            .run(|| async move {
                run_agent_revision_simulation_inner(pool, request, identity)
                    .await
                    .map(Json::from)
            })
            .name("experiments_run_agent_revision_simulation")
            .await?
            .into_inner();
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<ExperimentRunClient>(accepted.run_uid.to_string())
                .run(Json::from(accepted.workflow_request())),
        )
        .send();
        Ok(Json::from(accepted.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn compare_agent_revision_simulation(
        &self,
        ctx: Context<'_>,
        request: Json<AgentRevisionSimulationCompareRequest>,
    ) -> Result<Json<AgentRevisionSimulationCompareResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "compare_agent_revision_simulation");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                compare_agent_revision_simulation_inner(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("experiments_compare_agent_revision_simulation")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn compare_agent_revisions(
        &self,
        ctx: Context<'_>,
        request: Json<AgentRevisionCompareRequest>,
    ) -> Result<Json<AgentRevisionCompareResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Experiments", "compare_agent_revisions");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                compare_agent_revisions_inner(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("experiments_compare_agent_revisions")
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

    match store_generated_plan(pool.clone(), request.clone(), &completion.text).await {
        Ok(response) => Ok(response),
        // Only generation defects are model-repairable; storage/infra errors
        // would bill a second completion just to fail the same way again.
        Err(initial_error @ ExperimentAppError::BadRequest(_)) => {
            let repair = plan_generation_repair_request(
                &request,
                &completion.text,
                &initial_error.to_string(),
            )
            .map_err(experiment_app_error_to_handler_error)?;
            let repaired = gateway
                .complete_buffered(repair)
                .await
                .map_err(moa_error_to_handler_error)?;
            store_generated_plan(pool, request, &repaired.text)
                .await
                .map_err(experiment_app_error_to_handler_error)
        }
        Err(error) => Err(experiment_app_error_to_handler_error(error)),
    }
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

/// Lists visible behavior-lab plan artifacts after caller authorization has passed.
pub async fn list_plans_inner(
    pool: sqlx::PgPool,
    request: ExperimentPlanListRequest,
) -> Result<ExperimentPlanListResponse, HandlerError> {
    let scope = request.scope.unwrap_or(ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    });
    if scope.tenant_id() != request.tenant_id {
        return Err(TerminalError::new_with_code(
            400,
            "plan list scope tenant_id must match request tenant_id",
        )
        .into());
    }
    let status = request
        .status
        .as_deref()
        .map(str::parse::<ArtifactStatus>)
        .transpose()
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()))?;
    let plans = ArtifactRegistry::new(pool)
        .list_visible(&scope, Some(ArtifactKind::ExperimentPlan), status)
        .await
        .map_err(moa_error_to_handler_error)?
        .into_iter()
        .map(|summary| WireArtifactSummary {
            artifact_uid: summary.artifact_uid,
            revision_uid: summary.revision_uid,
            scope: summary.scope,
            kind: summary.kind.to_string(),
            name: summary.name,
            description: summary.description,
            tags: summary.tags,
            status: summary.status.to_string(),
            version: summary.version,
            updated_at: summary.updated_at,
        })
        .collect();

    Ok(ExperimentPlanListResponse {
        tenant_id: request.tenant_id,
        plans,
    })
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
    identity: Identity,
) -> Result<ExperimentCancelResponse, HandlerError> {
    cancel_run(pool, request, identity)
        .await
        .map_err(experiment_app_error_to_handler_error)
}

async fn propose_improvements_inner(
    pool: sqlx::PgPool,
    session_store: Arc<dyn LearningCandidateStore>,
    request: ExperimentProposeImprovementsRequest,
) -> Result<ExperimentProposeImprovementsResponse, HandlerError> {
    let proposal = propose_improvement_candidate(pool, request)
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

/// Internal accepted-run payload for an agent-revision simulation admission.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentRevisionSimulationAccepted {
    /// Tenant that owns the accepted run.
    tenant_id: TenantId,
    /// Durable experiment run identifier.
    run_uid: uuid::Uuid,
    /// Experiment-plan revision expanded by the run.
    plan_revision_uid: uuid::Uuid,
    /// Identity that admitted the run.
    identity: Identity,
    /// Run-level score run identifier.
    score_run_id: uuid::Uuid,
    /// Exact agent revision variants accepted for the run.
    variants: Vec<AgentRevisionSimulationVariant>,
    /// Public response payload returned by the Restate handler.
    response: AgentRevisionSimulationRunResponse,
}

impl AgentRevisionSimulationAccepted {
    /// Builds the workflow dispatch request for the accepted simulation run.
    #[must_use]
    pub fn workflow_request(&self) -> ExperimentRunWorkflowRequest {
        ExperimentRunWorkflowRequest {
            tenant_id: self.tenant_id,
            run_uid: self.run_uid,
            target: serde_json::json!({}),
            variant: serde_json::json!({}),
            plan_revision_uid: Some(self.plan_revision_uid),
            identity: self.identity.clone(),
            score_run_id: self.score_run_id,
            agent_revision_variants: self.variants.clone(),
        }
    }
}

/// Admits an agent-revision simulation run after caller authorization has passed.
pub async fn run_agent_revision_simulation_inner(
    pool: sqlx::PgPool,
    request: AgentRevisionSimulationRunRequest,
    identity: Identity,
) -> Result<AgentRevisionSimulationAccepted, HandlerError> {
    let variants = simulation_variants(request.base, request.candidates)?;
    let admitted = run_inner(
        pool,
        ExperimentRunRequest {
            tenant_id: request.tenant_id,
            name: request.name,
            plan_revision_uid: Some(request.plan_revision_uid),
            target: None,
            variant: None,
            // Plan-backed: the pinned plan revision owns the scorecard, so this
            // path must not supply one that would compete with it.
            scorecard: None,
            score_run_id: None,
            idempotency_key: request.idempotency_key,
            agent_revision_variants: variants.clone(),
        },
        identity.clone(),
    )
    .await?;
    Ok(AgentRevisionSimulationAccepted {
        tenant_id: request.tenant_id,
        run_uid: admitted.run_uid,
        plan_revision_uid: request.plan_revision_uid,
        identity,
        score_run_id: admitted.score_run_id,
        variants: variants.clone(),
        response: AgentRevisionSimulationRunResponse {
            tenant_id: request.tenant_id,
            run_uid: admitted.run_uid,
            status: admitted.response.status,
            score_run_id: admitted.score_run_id,
            plan_revision_uid: request.plan_revision_uid,
            variants,
        },
    })
}

/// Compares agent-revision simulation variants after caller authorization has passed.
pub async fn compare_agent_revision_simulation_inner(
    pool: sqlx::PgPool,
    request: AgentRevisionSimulationCompareRequest,
) -> Result<AgentRevisionSimulationCompareResponse, HandlerError> {
    let scope = tenant_scope(request.tenant_id);
    let store = ExperimentStore::new(pool.clone());
    let run = store
        .load_run(&scope, request.run_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "simulation run not found"))?;
    let variants = variants_from_run_metadata(&run.variant)?;
    let candidate_keys = if request.candidate_variant_keys.is_empty() {
        variants
            .iter()
            .filter(|variant| variant.variant_key != request.base_variant_key)
            .map(|variant| variant.variant_key.clone())
            .collect::<Vec<_>>()
    } else {
        request.candidate_variant_keys.clone()
    };
    let mut wanted = BTreeSet::from([request.base_variant_key.clone()]);
    wanted.extend(candidate_keys.iter().cloned());
    let variant_revisions = variants
        .into_iter()
        .map(|variant| (variant.variant_key, variant.revision_uid))
        .collect::<BTreeMap<_, _>>();
    for key in &wanted {
        if !variant_revisions.contains_key(key) {
            return Err(TerminalError::new_with_code(
                400,
                format!("simulation variant `{key}` was not part of the run"),
            )
            .into());
        }
    }

    let trials = store
        .list_trials(&scope, request.run_uid, None, 100_000)
        .await
        .map_err(moa_error_to_handler_error)?;
    let mut summaries = wanted
        .iter()
        .map(|key| {
            (
                key.clone(),
                MutableSimulationVariantResult::new(
                    key.clone(),
                    *variant_revisions.get(key).expect("checked above"),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for trial in &trials {
        let Some(summary) = summaries.get_mut(&trial.variant_key) else {
            continue;
        };
        summary.record_trial(trial);
    }

    let breakdown = experiment_score_breakdown_for_tenant(
        &pool,
        ExperimentRunScoreRef {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    let variant_deltas = simulation_variant_deltas(
        &breakdown.trials,
        &request.base_variant_key,
        &candidate_keys,
    );

    Ok(AgentRevisionSimulationCompareResponse {
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        base_variant_key: request.base_variant_key,
        variants: summaries.into_values().map(Into::into).collect(),
        variant_deltas,
    })
}

fn simulation_variants(
    base: AgentRevisionSimulationVariant,
    candidates: Vec<AgentRevisionSimulationVariant>,
) -> Result<Vec<AgentRevisionSimulationVariant>, HandlerError> {
    let mut variants = vec![base];
    variants.extend(candidates);
    if variants.len() < 2 {
        return Err(TerminalError::new_with_code(
            400,
            "agent revision simulation requires at least one candidate",
        )
        .into());
    }
    let mut keys = BTreeSet::new();
    for variant in &variants {
        if variant.variant_key.trim().is_empty() {
            return Err(
                TerminalError::new_with_code(400, "simulation variant_key is required").into(),
            );
        }
        if !keys.insert(variant.variant_key.clone()) {
            return Err(TerminalError::new_with_code(
                400,
                format!("duplicate simulation variant_key `{}`", variant.variant_key),
            )
            .into());
        }
    }
    Ok(variants)
}

fn variants_from_run_metadata(
    variant: &ExperimentVariant,
) -> Result<Vec<AgentRevisionSimulationVariant>, HandlerError> {
    let value = variant
        .metadata
        .get("agent_revision_variants")
        .ok_or_else(|| {
            TerminalError::new_with_code(
                400,
                "experiment run does not contain agent revision simulation variants",
            )
        })?;
    serde_json::from_value(value.clone())
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

#[derive(Debug, Clone)]
struct MutableSimulationVariantResult {
    variant_key: String,
    revision_uid: uuid::Uuid,
    trial_count: u64,
    completed_count: u64,
    failed_count: u64,
    cancelled_count: u64,
    score_run_ids: BTreeSet<uuid::Uuid>,
    session_ids: Vec<moa_core::types::identifiers::SessionId>,
    stop_reason_counts: BTreeMap<String, u64>,
    errors: BTreeSet<String>,
}

impl MutableSimulationVariantResult {
    fn new(variant_key: String, revision_uid: uuid::Uuid) -> Self {
        Self {
            variant_key,
            revision_uid,
            trial_count: 0,
            completed_count: 0,
            failed_count: 0,
            cancelled_count: 0,
            score_run_ids: BTreeSet::new(),
            session_ids: Vec::new(),
            stop_reason_counts: BTreeMap::new(),
            errors: BTreeSet::new(),
        }
    }

    fn record_trial(&mut self, trial: &moa_experiments::model::ExperimentTrialRecord) {
        self.trial_count = self.trial_count.saturating_add(1);
        match trial.status {
            ExperimentTrialStatus::Completed => {
                self.completed_count = self.completed_count.saturating_add(1);
            }
            ExperimentTrialStatus::Failed => {
                self.failed_count = self.failed_count.saturating_add(1);
            }
            ExperimentTrialStatus::Cancelled => {
                self.cancelled_count = self.cancelled_count.saturating_add(1);
            }
            ExperimentTrialStatus::Accepted
            | ExperimentTrialStatus::Dispatched
            | ExperimentTrialStatus::Running => {}
        }
        self.score_run_ids.insert(trial.score_run_id);
        if let Some(session_id) = trial.session_id
            && !self.session_ids.contains(&session_id)
        {
            self.session_ids.push(session_id);
        }
        if let Some(stop_reason) = trial.stop_reason {
            *self
                .stop_reason_counts
                .entry(stop_reason.as_str().to_string())
                .or_default() += 1;
        }
        if let Some(error) = trial.error.as_ref().filter(|error| !error.is_empty()) {
            self.errors.insert(error.clone());
        }
    }
}

impl From<MutableSimulationVariantResult> for AgentRevisionSimulationVariantResult {
    fn from(value: MutableSimulationVariantResult) -> Self {
        Self {
            variant_key: value.variant_key,
            revision_uid: value.revision_uid,
            trial_count: value.trial_count,
            completed_count: value.completed_count,
            failed_count: value.failed_count,
            cancelled_count: value.cancelled_count,
            score_run_ids: value.score_run_ids.into_iter().collect(),
            session_ids: value.session_ids,
            stop_reason_counts: value.stop_reason_counts,
            errors: value.errors.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ScoreAccumulator {
    weighted_sum: f64,
    count: u64,
}

impl ScoreAccumulator {
    fn add(&mut self, mean: Option<f64>, count: u64) {
        let Some(mean) = mean else {
            return;
        };
        self.weighted_sum += mean * count as f64;
        self.count = self.count.saturating_add(count);
    }

    fn mean(&self) -> Option<f64> {
        (self.count > 0).then_some(self.weighted_sum / self.count as f64)
    }
}

fn simulation_variant_deltas(
    trials: &[TrialScoreSummary],
    base_variant_key: &str,
    candidate_variant_keys: &[String],
) -> Vec<ExperimentVariantScoreDeltaRow> {
    let candidate_keys = candidate_variant_keys
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut aggregates: BTreeMap<String, BTreeMap<String, ScoreAccumulator>> = BTreeMap::new();
    for trial in trials {
        if trial.variant_key != base_variant_key && !candidate_keys.contains(&trial.variant_key) {
            continue;
        }
        let by_score = aggregates.entry(trial.variant_key.clone()).or_default();
        for row in &trial.rows {
            by_score
                .entry(row.name.clone())
                .or_default()
                .add(row.mean_or_rate, row.n);
        }
    }
    let base = aggregates
        .get(base_variant_key)
        .cloned()
        .unwrap_or_default();
    let mut deltas = Vec::new();
    for candidate_key in candidate_variant_keys {
        let Some(candidate) = aggregates.get(candidate_key) else {
            continue;
        };
        let score_names = base
            .keys()
            .chain(candidate.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in score_names {
            let base_mean = base.get(&name).and_then(ScoreAccumulator::mean);
            let new_mean = candidate.get(&name).and_then(ScoreAccumulator::mean);
            deltas.push(ExperimentVariantScoreDeltaRow {
                variant_key: candidate_key.clone(),
                name,
                base_mean,
                new_mean,
                delta: new_mean.zip(base_mean).map(|(new, base)| new - base),
            });
        }
    }
    deltas
}

async fn compare_agent_revisions_inner(
    pool: sqlx::PgPool,
    request: AgentRevisionCompareRequest,
) -> Result<AgentRevisionCompareResponse, HandlerError> {
    let resolver = AgentResolver::new(pool);
    let scope = tenant_scope(request.tenant_id);
    let (base, new) = tokio::try_join!(
        resolver.resolve_exact_revision(&scope, request.base_revision_uid),
        resolver.resolve_exact_revision(&scope, request.new_revision_uid),
    )
    .map_err(moa_error_to_handler_error)?;
    Ok(compare_agent_revision_policies(request, &base, &new))
}

fn compare_agent_revision_policies(
    request: AgentRevisionCompareRequest,
    base: &AgentRuntimePolicy,
    new: &AgentRuntimePolicy,
) -> AgentRevisionCompareResponse {
    let artifact_dependency_deltas =
        compare_artifact_dependencies(&base.revision_lock, &new.revision_lock);
    let tool_dependency_deltas = compare_tool_dependencies(&base.revision_lock, &new.revision_lock);
    let instructions_changed = base.instructions != new.instructions;
    let tool_policy_changed = base.tool_policy != new.tool_policy;
    let changed = base.revision_lock.canonical_policy_hash
        != new.revision_lock.canonical_policy_hash
        || instructions_changed
        || tool_policy_changed
        || !artifact_dependency_deltas.is_empty()
        || !tool_dependency_deltas.is_empty();

    AgentRevisionCompareResponse {
        tenant_id: request.tenant_id,
        base_revision_uid: request.base_revision_uid,
        new_revision_uid: request.new_revision_uid,
        base_policy_hash: base.revision_lock.canonical_policy_hash.clone(),
        new_policy_hash: new.revision_lock.canonical_policy_hash.clone(),
        changed,
        instructions_changed,
        tool_policy_changed,
        artifact_dependency_deltas,
        tool_dependency_deltas,
    }
}

fn tenant_scope(tenant_id: TenantId) -> ActionRuleScope {
    ActionRuleScope::Tenant { tenant_id }
}

fn compare_artifact_dependencies(
    base: &moa_core::types::agent::AgentRevisionLock,
    new: &moa_core::types::agent::AgentRevisionLock,
) -> Vec<AgentArtifactDependencyDelta> {
    let mut references = BTreeMap::new();
    for dependency in &base.artifact_dependencies {
        references
            .entry(dependency.reference.clone())
            .or_insert((None, None))
            .0 = Some(dependency.revision_uid);
    }
    for dependency in &new.artifact_dependencies {
        references
            .entry(dependency.reference.clone())
            .or_insert((None, None))
            .1 = Some(dependency.revision_uid);
    }
    references
        .into_iter()
        .filter_map(|(reference, (base_revision_uid, new_revision_uid))| {
            dependency_change(base_revision_uid, new_revision_uid).map(|change| {
                AgentArtifactDependencyDelta {
                    reference,
                    base_revision_uid,
                    new_revision_uid,
                    change,
                }
            })
        })
        .collect()
}

fn compare_tool_dependencies(
    base: &moa_core::types::agent::AgentRevisionLock,
    new: &moa_core::types::agent::AgentRevisionLock,
) -> Vec<AgentToolDependencyDelta> {
    let mut tools = BTreeMap::new();
    for dependency in &base.tool_dependencies {
        tools
            .entry(dependency.name.clone())
            .or_insert((None, None))
            .0 = Some(dependency.identity_hash.clone());
    }
    for dependency in &new.tool_dependencies {
        tools
            .entry(dependency.name.clone())
            .or_insert((None, None))
            .1 = Some(dependency.identity_hash.clone());
    }
    tools
        .into_iter()
        .filter_map(|(name, (base_identity_hash, new_identity_hash))| {
            dependency_change(base_identity_hash.as_deref(), new_identity_hash.as_deref()).map(
                |change| AgentToolDependencyDelta {
                    name,
                    base_identity_hash,
                    new_identity_hash,
                    change,
                },
            )
        })
        .collect()
}

fn dependency_change<T: Eq>(base: Option<T>, new: Option<T>) -> Option<AgentDependencyChange> {
    match (base, new) {
        (None, Some(_)) => Some(AgentDependencyChange::Added),
        (Some(_), None) => Some(AgentDependencyChange::Removed),
        (Some(base), Some(new)) if base != new => Some(AgentDependencyChange::Changed),
        _ => None,
    }
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

fn score_error_to_handler_error(error: Error) -> HandlerError {
    match error {
        Error::IntegerTooLarge { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        Error::Sql(_) | Error::InvalidScoreValueType { .. } | Error::ScoreRunMismatch { .. } => {
            TerminalError::new(error.to_string()).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        types::agent::AgentRevisionLock, types::agent::LockedToolRef,
        types::agent::ResolvedArtifactRevisionRef,
    };
    use moa_wire::experiments::AgentDependencyChange;
    use uuid::Uuid;

    use super::{compare_artifact_dependencies, compare_tool_dependencies};

    #[test]
    fn agent_revision_compare_reports_exact_dependency_deltas() {
        // Pins: revision comparison reports exact artifact and tool lock changes for simulation review.
        let base_skill = Uuid::now_v7();
        let new_skill = Uuid::now_v7();
        let base = AgentRevisionLock {
            agent_revision_uid: Uuid::now_v7(),
            artifact_dependencies: vec![artifact_dependency("skill://support", base_skill)],
            tool_dependencies: vec![tool_dependency("file_read", "hash-a")],
            canonical_policy_hash: "base-hash".to_string(),
        };
        let new = AgentRevisionLock {
            agent_revision_uid: Uuid::now_v7(),
            artifact_dependencies: vec![
                artifact_dependency("skill://support", new_skill),
                artifact_dependency("skill://refund", Uuid::now_v7()),
            ],
            tool_dependencies: vec![tool_dependency("file_read", "hash-b")],
            canonical_policy_hash: "new-hash".to_string(),
        };

        let artifact_deltas = compare_artifact_dependencies(&base, &new);
        assert_eq!(artifact_deltas.len(), 2);
        assert_eq!(artifact_deltas[0].reference, "skill://refund");
        assert_eq!(artifact_deltas[0].change, AgentDependencyChange::Added);
        assert_eq!(artifact_deltas[1].reference, "skill://support");
        assert_eq!(artifact_deltas[1].base_revision_uid, Some(base_skill));
        assert_eq!(artifact_deltas[1].new_revision_uid, Some(new_skill));
        assert_eq!(artifact_deltas[1].change, AgentDependencyChange::Changed);

        let tool_deltas = compare_tool_dependencies(&base, &new);
        assert_eq!(tool_deltas.len(), 1);
        assert_eq!(tool_deltas[0].name, "file_read");
        assert_eq!(tool_deltas[0].base_identity_hash.as_deref(), Some("hash-a"));
        assert_eq!(tool_deltas[0].new_identity_hash.as_deref(), Some("hash-b"));
        assert_eq!(tool_deltas[0].change, AgentDependencyChange::Changed);
    }

    fn artifact_dependency(reference: &str, revision_uid: Uuid) -> ResolvedArtifactRevisionRef {
        ResolvedArtifactRevisionRef {
            reference: reference.to_string(),
            kind: reference
                .split_once("://")
                .map(|(kind, _)| kind)
                .unwrap_or("skill")
                .to_string(),
            name: reference
                .split_once("://")
                .map(|(_, name)| name)
                .unwrap_or(reference)
                .to_string(),
            artifact_uid: Uuid::now_v7(),
            revision_uid,
            version: 1,
        }
    }

    fn tool_dependency(name: &str, identity_hash: &str) -> LockedToolRef {
        LockedToolRef {
            name: name.to_string(),
            identity_hash: identity_hash.to_string(),
            provider: None,
        }
    }
}
