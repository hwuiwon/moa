//! Restate service for authorized live behavior experiment run metadata.

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::simulation::experiment_plan_response_schema;
use moa_artifacts::validation::{ValidationReport, validate_for_status};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::{
    ExperimentCancelRequest, ExperimentCancelResponse, ExperimentCompareRequest,
    ExperimentCompareResponse, ExperimentCompareRow, ExperimentGeneratePlanRequest,
    ExperimentGeneratePlanResponse, ExperimentListRequest, ExperimentListResponse,
    ExperimentProposeImprovementsRequest, ExperimentProposeImprovementsResponse,
    ExperimentRunRequest, ExperimentRunResponse, ExperimentRunStatusRequest,
    ExperimentRunStatusResponse, ExperimentScenarioScoreDeltaRow, ExperimentScenarioScoreSummary,
    ExperimentScoreSummaryRow, ExperimentScoresRequest, ExperimentScoresResponse,
    ExperimentTrialScoreSummary, ExperimentTrialStatusRequest, ExperimentTrialStatusResponse,
    ExperimentTrialSummary, ExperimentTrialsRequest, ExperimentTrialsResponse,
    ExperimentVariantScoreDeltaRow,
};
use moa_core::{
    CompletionRequest, ContextMessage, JsonResponseFormat, LearningCandidate,
    LearningCandidateStatus, LearningCandidateType, LearningRiskClass, MemoryScope, MoaError,
    ModelId, WorkspaceId, record_experiment_learning_candidates, record_experiment_run,
    record_experiment_score_rows,
};
use moa_experiments::model::{
    ExperimentRunRecord, ExperimentRunStatus, ExperimentScorecard, ExperimentTarget,
    ExperimentTrialRecord, ExperimentTrialStatus, ExperimentVariant, NewExperimentRun,
};
use moa_experiments::plan::{PlanExpansionError, project_plan_run};
use moa_experiments::store::ExperimentStore;
use moa_scoring::{
    ExperimentRunCompareRef, ExperimentRunScoreRef, ScenarioScoreDeltaRow, ScenarioScoreSummary,
    ScoreCompareRef, ScoreCompareRow, ScoreRunRef, ScoreSummary, ScoreSummaryRow, ScoringError,
    TrialScoreSummary, VariantScoreDeltaRow, compare_experiment_score_breakdown_for_workspace,
    compare_score_runs_for_workspace, experiment_score_breakdown_for_workspace,
    score_summaries_for_workspace,
};
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::llm_gateway::LLMGatewayImpl;
use crate::workflows::experiment_run::{ExperimentRunClient, ExperimentRunWorkflowRequest};

const DEFAULT_LIST_LIMIT: i64 = 50;
const GENERATED_PLAN_SOURCE_FORMAT: &str = "json";

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
        let pool = runtime.graph_pool.clone();
        let gateway = LLMGatewayImpl::new(runtime.providers.clone());

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
    async fn trials(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentTrialsRequest>,
    ) -> Result<Json<ExperimentTrialsResponse>, HandlerError> {
        annotate_restate_handler_span("Experiments", "trials");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        let pool = OrchestratorCtx::current().graph_pool.clone();

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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let runtime = OrchestratorCtx::current();
        let pool = runtime.graph_pool.clone();
        let session_store = runtime.session_store.clone();

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

async fn generate_plan_inner(
    pool: sqlx::PgPool,
    gateway: LLMGatewayImpl,
    request: ExperimentGeneratePlanRequest,
) -> Result<ExperimentGeneratePlanResponse, HandlerError> {
    if request.description.trim().is_empty() {
        return Err(bad_request("experiment plan description must not be empty"));
    }

    let completion = gateway
        .complete_buffered(plan_generation_request(&request)?)
        .await
        .map_err(moa_error_to_handler_error)?;
    let document = parse_generated_plan_document(&completion.text)?;
    require_valid_generated_plan(&document)?;
    let source_text = document.to_json().map_err(artifact_doc_error)?;
    let document_value = serde_json::to_value(&document).map_err(|error| {
        HandlerError::from(TerminalError::new(format!(
            "serialize generated plan failed: {error}"
        )))
    })?;
    let scope = workspace_scope(request.workspace_id.clone());
    let stored = ArtifactRegistry::new(pool)
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: GENERATED_PLAN_SOURCE_FORMAT,
                source_text: source_text.as_bytes(),
                files: &[],
            },
        )
        .await
        .map_err(moa_error_to_handler_error)?;

    Ok(ExperimentGeneratePlanResponse {
        workspace_id: request.workspace_id,
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        status: stored.status.to_string(),
        source_format: stored.source_format,
        source_text,
        document: document_value,
        validation_report: stored.validation_report,
    })
}

async fn run_inner(
    pool: sqlx::PgPool,
    request: ExperimentRunRequest,
    identity: Identity,
) -> Result<AcceptedExperimentRun, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let run_inputs = match request.plan_revision_uid {
        Some(plan_revision_uid) => {
            plan_run_inputs(pool.clone(), &scope, plan_revision_uid, &request.name).await?
        }
        None => single_target_run_inputs(request.target, request.variant, request.scorecard)?,
    };
    let score_run_id = request.score_run_id.unwrap_or_else(Uuid::now_v7);
    let run = ExperimentStore::new(pool)
        .insert_run(
            &scope,
            NewExperimentRun {
                name: request.name,
                session_id: session_id_from_target(&run_inputs.target),
                workflow_run_uid: None,
                artifact_revision_uids: run_inputs.artifact_revision_uids.clone(),
                score_run_id,
                target: run_inputs.target,
                variant: run_inputs.variant,
                scorecard: run_inputs.scorecard,
                idempotency_key: request.idempotency_key,
                created_by_identity: identity_payload(identity.clone())?,
            },
        )
        .await
        .map_err(moa_error_to_handler_error)?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());

    let workflow_request = ExperimentRunWorkflowRequest {
        workspace_id: request.workspace_id.clone(),
        run_uid: run.run_uid,
        target: serialized_payload("target", &run.target)?,
        variant: serialized_payload("variant", &run.variant)?,
        plan_revision_uid: run_inputs.plan_revision_uid,
        identity,
        score_run_id: run.score_run_id,
    };

    Ok(AcceptedExperimentRun {
        response: run_response_from_record(request.workspace_id, &run),
        workflow_request,
    })
}

#[derive(Debug, Clone)]
struct ExperimentRunInputs {
    target: ExperimentTarget,
    variant: ExperimentVariant,
    scorecard: ExperimentScorecard,
    artifact_revision_uids: Vec<Uuid>,
    plan_revision_uid: Option<Uuid>,
}

fn single_target_run_inputs(
    target: Option<Value>,
    variant: Option<Value>,
    scorecard: Value,
) -> Result<ExperimentRunInputs, HandlerError> {
    let target = parse_payload::<ExperimentTarget>(
        "target",
        target.ok_or_else(|| bad_request("experiment target is required without a plan"))?,
    )?;
    let variant = parse_payload::<ExperimentVariant>(
        "variant",
        variant.ok_or_else(|| bad_request("experiment variant is required without a plan"))?,
    )?;
    let scorecard = parse_payload::<ExperimentScorecard>("scorecard", scorecard)?;
    Ok(ExperimentRunInputs {
        artifact_revision_uids: variant.artifact_revision_uids.clone(),
        target,
        variant,
        scorecard,
        plan_revision_uid: None,
    })
}

async fn plan_run_inputs(
    pool: sqlx::PgPool,
    scope: &MemoryScope,
    plan_revision_uid: Uuid,
    run_name: &str,
) -> Result<ExperimentRunInputs, HandlerError> {
    let plan = load_published_plan_revision(pool, scope, plan_revision_uid).await?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan.document.definition else {
        return Err(bad_request(
            "plan revision must contain an experiment_plan definition",
        ));
    };
    let projection = project_plan_run(definition, plan_revision_uid, &plan.name, run_name)
        .map_err(plan_expansion_error_to_handler_error)?;
    Ok(ExperimentRunInputs {
        target: projection.target,
        variant: projection.variant,
        scorecard: projection.scorecard,
        artifact_revision_uids: projection.artifact_revision_uids,
        plan_revision_uid: Some(projection.plan_revision_uid),
    })
}

async fn load_published_plan_revision(
    pool: sqlx::PgPool,
    scope: &MemoryScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision, HandlerError> {
    let revision = ArtifactRegistry::new(pool)
        .load_revision(scope, revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| bad_request(format!("experiment plan revision {revision_uid} not found")))?;
    if revision.kind != ArtifactKind::ExperimentPlan {
        return Err(bad_request(format!(
            "artifact revision {revision_uid} has kind {}, expected experiment_plan",
            revision.kind
        )));
    }
    if revision.status != ArtifactStatus::Published {
        return Err(bad_request(format!(
            "experiment plan revision {revision_uid} must be published before execution"
        )));
    }
    Ok(revision)
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

async fn trials_inner(
    pool: sqlx::PgPool,
    request: ExperimentTrialsRequest,
) -> Result<ExperimentTrialsResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let status = request
        .status
        .as_deref()
        .map(parse_trial_status)
        .transpose()?;
    let limit = request
        .limit
        .map(|limit| i64::try_from(limit).map_err(|_| bad_request("limit is too large")))
        .transpose()?
        .unwrap_or(DEFAULT_LIST_LIMIT);
    let trials = ExperimentStore::new(pool)
        .list_trials(&scope, request.run_uid, status, limit)
        .await
        .map_err(moa_error_to_handler_error)?
        .into_iter()
        .map(|trial| trial_summary_from_record(request.workspace_id.clone(), &trial))
        .collect();

    Ok(ExperimentTrialsResponse {
        workspace_id: request.workspace_id,
        run_uid: request.run_uid,
        trials,
    })
}

async fn trial_status_inner(
    pool: sqlx::PgPool,
    request: ExperimentTrialStatusRequest,
) -> Result<ExperimentTrialStatusResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let trial = ExperimentStore::new(pool)
        .load_trial(&scope, request.trial_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| trial_not_found(request.trial_uid))?;
    let summary = trial_summary_from_record(request.workspace_id, &trial);

    Ok(ExperimentTrialStatusResponse {
        workspace_id: summary.workspace_id,
        run_uid: summary.run_uid,
        trial_uid: summary.trial_uid,
        status: summary.status,
        target_kind: summary.target_kind,
        trial_key: summary.trial_key,
        variant_key: summary.variant_key,
        scenario_id: summary.scenario_id,
        score_run_id: summary.score_run_id,
        session_id: summary.session_id,
        workflow_run_uid: summary.workflow_run_uid,
        trace_id: summary.trace_id,
        stop_reason: summary.stop_reason,
        error: summary.error,
        turn_count: summary.turn_count,
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
    let store = ExperimentStore::new(pool);
    let run = store
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
    store
        .cancel_active_trials(&scope, request.run_uid, reason.clone())
        .await
        .map_err(moa_error_to_handler_error)?;
    record_experiment_run(run.status.as_str(), run.target_kind.as_str());

    Ok(ExperimentCancelResponse {
        workspace_id: request.workspace_id,
        run_uid: run.run_uid,
        cancelled: true,
        status: run.status.as_str().to_string(),
        reason,
    })
}

async fn propose_improvements_inner(
    pool: sqlx::PgPool,
    session_store: Arc<PostgresSessionStore>,
    request: ExperimentProposeImprovementsRequest,
) -> Result<ExperimentProposeImprovementsResponse, HandlerError> {
    let scope = workspace_scope(request.workspace_id.clone());
    let store = ExperimentStore::new(pool.clone());
    let run = load_required_run(&store, &scope, request.run_uid).await?;
    require_completed_run(&run)?;
    let plan_revision_uid = require_proposal_enabled_plan(pool.clone(), &scope, &run).await?;
    let completed_trials = store
        .list_trials(
            &scope,
            run.run_uid,
            Some(ExperimentTrialStatus::Completed),
            10_000,
        )
        .await
        .map_err(moa_error_to_handler_error)?;
    if completed_trials.is_empty() {
        return Err(bad_request(
            "experiment learning proposals require at least one completed trial",
        ));
    }
    let run_score_summary = score_summaries_for_workspace(
        &pool,
        ScoreRunRef {
            workspace_id: request.workspace_id.clone(),
            run_id: run.score_run_id,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    if run_score_summary.rows.is_empty() {
        return Err(bad_request(
            "experiment learning proposals require score rows for the run score run",
        ));
    }
    let trial_breakdown = experiment_score_breakdown_for_workspace(
        &pool,
        ExperimentRunScoreRef {
            workspace_id: request.workspace_id.clone(),
            run_uid: run.run_uid,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    require_trial_score_rows(&completed_trials, &trial_breakdown.trials)?;

    let draft_artifact_revision_uids = Vec::new();
    let candidate = build_experiment_learning_candidate(ExperimentLearningProposalEvidence {
        workspace_id: request.workspace_id.clone(),
        run: &run,
        completed_trials: &completed_trials,
        run_score_summary: &run_score_summary,
        trial_rollup_rows: &trial_breakdown.trial_rollup_rows,
        trial_score_summaries: &trial_breakdown.trials,
        scenario_score_summaries: &trial_breakdown.scenarios,
        plan_revision_uid,
        draft_artifact_revision_uids: &draft_artifact_revision_uids,
        idempotency_key: request.idempotency_key.as_deref(),
        now: Utc::now(),
    });
    session_store
        .append_learning_candidate(&candidate)
        .await
        .map_err(moa_error_to_handler_error)?;
    record_experiment_learning_candidates(candidate.status.as_str(), 1);

    Ok(ExperimentProposeImprovementsResponse {
        workspace_id: request.workspace_id,
        run_uid: run.run_uid,
        candidate_ids: vec![candidate.id],
        draft_artifact_revision_uids,
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
        ScoreRunRef {
            workspace_id: request.workspace_id.clone(),
            run_id: run.score_run_id,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    record_experiment_score_rows("scores", score_response.rows.len() as u64);
    let trial_breakdown = experiment_score_breakdown_for_workspace(
        &pool,
        ExperimentRunScoreRef {
            workspace_id: request.workspace_id.clone(),
            run_uid: run.run_uid,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    record_experiment_score_rows(
        "trial_rollup",
        trial_breakdown.trial_rollup_rows.len() as u64,
    );
    record_experiment_score_rows(
        "trial_breakdown",
        trial_breakdown
            .trials
            .iter()
            .map(|trial| trial.rows.len() as u64)
            .sum(),
    );
    record_experiment_score_rows(
        "scenario_breakdown",
        trial_breakdown
            .scenarios
            .iter()
            .map(|scenario| scenario.rows.len() as u64)
            .sum(),
    );

    Ok(ExperimentScoresResponse {
        workspace_id: request.workspace_id,
        run_uid: run.run_uid,
        score_run_id: run.score_run_id,
        rows: score_response
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
        trial_rollup_rows: trial_breakdown
            .trial_rollup_rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
        trials: trial_breakdown
            .trials
            .into_iter()
            .map(experiment_trial_score_summary)
            .collect(),
        scenarios: trial_breakdown
            .scenarios
            .into_iter()
            .map(experiment_scenario_score_summary)
            .collect(),
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
        ScoreCompareRef {
            workspace_id: request.workspace_id.clone(),
            base_run: base_run.score_run_id,
            new_run: new_run.score_run_id,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    record_experiment_score_rows("compare", compare_response.rows.len() as u64);
    let breakdown_compare = compare_experiment_score_breakdown_for_workspace(
        &pool,
        ExperimentRunCompareRef {
            workspace_id: request.workspace_id.clone(),
            base_run_uid: base_run.run_uid,
            new_run_uid: new_run.run_uid,
        },
    )
    .await
    .map_err(score_error_to_handler_error)?;
    record_experiment_score_rows(
        "scenario_compare",
        breakdown_compare.scenario_deltas.len() as u64,
    );
    record_experiment_score_rows(
        "variant_compare",
        breakdown_compare.variant_deltas.len() as u64,
    );

    Ok(ExperimentCompareResponse {
        workspace_id: request.workspace_id,
        base_run_uid: base_run.run_uid,
        new_run_uid: new_run.run_uid,
        base_score_run_id: base_run.score_run_id,
        new_score_run_id: new_run.score_run_id,
        rows: compare_response
            .rows
            .into_iter()
            .map(experiment_compare_row)
            .collect(),
        scenario_deltas: breakdown_compare
            .scenario_deltas
            .into_iter()
            .map(experiment_scenario_score_delta_row)
            .collect(),
        variant_deltas: breakdown_compare
            .variant_deltas
            .into_iter()
            .map(experiment_variant_score_delta_row)
            .collect(),
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

fn require_completed_run(run: &ExperimentRunRecord) -> Result<(), HandlerError> {
    if run.status != ExperimentRunStatus::Completed {
        return Err(bad_request(format!(
            "experiment run {} must be completed before proposing improvements",
            run.run_uid
        )));
    }
    Ok(())
}

async fn require_proposal_enabled_plan(
    pool: sqlx::PgPool,
    scope: &MemoryScope,
    run: &ExperimentRunRecord,
) -> Result<Uuid, HandlerError> {
    let registry = ArtifactRegistry::new(pool);
    let plan_revision_uid = experiment_plan_revision_uid(&registry, scope, run).await?;
    let plan = registry
        .load_revision(scope, plan_revision_uid)
        .await
        .map_err(moa_error_to_handler_error)?
        .ok_or_else(|| {
            bad_request(format!(
                "experiment plan revision {plan_revision_uid} is not visible"
            ))
        })?;
    let ArtifactDefinition::ExperimentPlan(definition) = &plan.document.definition else {
        return Err(bad_request(format!(
            "artifact revision {plan_revision_uid} is not an experiment_plan"
        )));
    };
    if !definition.learning_proposals.enabled {
        return Err(bad_request(format!(
            "experiment plan revision {plan_revision_uid} has learning_proposals.enabled=false"
        )));
    }
    Ok(plan_revision_uid)
}

async fn experiment_plan_revision_uid(
    registry: &ArtifactRegistry,
    scope: &MemoryScope,
    run: &ExperimentRunRecord,
) -> Result<Uuid, HandlerError> {
    if let Some(plan_revision_uid) = run
        .variant
        .metadata
        .get("plan_revision_uid")
        .and_then(Value::as_str)
        .map(|value| {
            Uuid::parse_str(value).map_err(|error| {
                bad_request(format!(
                    "experiment run {} has invalid plan_revision_uid metadata: {error}",
                    run.run_uid
                ))
            })
        })
        .transpose()?
    {
        return Ok(plan_revision_uid);
    }

    for revision_uid in &run.artifact_revision_uids {
        let Some(revision) = registry
            .load_revision(scope, *revision_uid)
            .await
            .map_err(moa_error_to_handler_error)?
        else {
            continue;
        };
        if revision.kind == ArtifactKind::ExperimentPlan {
            return Ok(*revision_uid);
        }
    }

    Err(bad_request(
        "experiment learning proposals require a proposal-enabled experiment plan revision",
    ))
}

fn require_trial_score_rows(
    completed_trials: &[ExperimentTrialRecord],
    score_trials: &[TrialScoreSummary],
) -> Result<(), HandlerError> {
    let scored_trial_ids = score_trials
        .iter()
        .filter(|trial| !trial.rows.is_empty())
        .map(|trial| trial.trial_uid)
        .collect::<std::collections::BTreeSet<_>>();
    let missing = completed_trials
        .iter()
        .find(|trial| !scored_trial_ids.contains(&trial.trial_uid));
    if let Some(trial) = missing {
        return Err(bad_request(format!(
            "experiment trial {} has no score rows",
            trial.trial_uid
        )));
    }
    Ok(())
}

/// Evidence used to build one experiment-derived learning candidate.
pub struct ExperimentLearningProposalEvidence<'a> {
    /// Workspace that owns the experiment run and candidate.
    pub workspace_id: WorkspaceId,
    /// Completed experiment run that produced proposal evidence.
    pub run: &'a ExperimentRunRecord,
    /// Completed trials attached to the experiment run.
    pub completed_trials: &'a [ExperimentTrialRecord],
    /// Score summary for the experiment run score run.
    pub run_score_summary: &'a ScoreSummary,
    /// Aggregate score rows across trial score runs.
    pub trial_rollup_rows: &'a [ScoreSummaryRow],
    /// Per-trial score summaries.
    pub trial_score_summaries: &'a [TrialScoreSummary],
    /// Per-scenario score summaries.
    pub scenario_score_summaries: &'a [ScenarioScoreSummary],
    /// Plan revision that explicitly enabled learning proposals.
    pub plan_revision_uid: Uuid,
    /// Draft artifact revisions created for suggested changes.
    pub draft_artifact_revision_uids: &'a [Uuid],
    /// Optional caller-provided idempotency key.
    pub idempotency_key: Option<&'a str>,
    /// Candidate creation timestamp.
    pub now: chrono::DateTime<Utc>,
}

/// Builds a proposed learning candidate from completed experiment evidence.
#[must_use]
pub fn build_experiment_learning_candidate(
    evidence: ExperimentLearningProposalEvidence<'_>,
) -> LearningCandidate {
    let candidate_id = deterministic_candidate_id(
        &evidence.workspace_id,
        evidence.run.run_uid,
        evidence.idempotency_key.unwrap_or("default"),
    );
    let payload = experiment_learning_candidate_payload(&evidence);

    LearningCandidate {
        id: candidate_id,
        tenant_id: evidence.workspace_id.to_string(),
        workspace_id: evidence.workspace_id,
        user_id: None,
        candidate_type: LearningCandidateType::Prompt,
        status: LearningCandidateStatus::Proposed,
        target_id: Some(format!("experiment_run:{}", evidence.run.run_uid)),
        target_label: Some(format!("Experiment proposal for {}", evidence.run.name)),
        task_fingerprint: None,
        task_facets: None,
        payload,
        evaluation_payload: None,
        source_experience_ids: Vec::new(),
        confidence: None,
        risk_class: LearningRiskClass::Medium,
        promotion_requirements: vec![
            "human_review".to_string(),
            "explicit_candidate_evaluation".to_string(),
            "no_automatic_artifact_publish".to_string(),
        ],
        status_reason: Some(
            "proposed from completed experiment evidence; no promotion performed".to_string(),
        ),
        batch_id: None,
        created_at: evidence.now,
        updated_at: evidence.now,
    }
}

fn experiment_learning_candidate_payload(
    evidence: &ExperimentLearningProposalEvidence<'_>,
) -> Value {
    let run = evidence.run;
    serde_json::json!({
        "kind": "experiment_learning_proposal",
        "source": "Experiments/propose_improvements",
        "workspace_id": evidence.workspace_id,
        "experiment": {
            "run_uid": run.run_uid,
            "name": run.name,
            "status": run.status.as_str(),
            "target_kind": run.target_kind.as_str(),
            "run_score_run_id": run.score_run_id,
            "session_id": run.session_id,
            "workflow_run_uid": run.workflow_run_uid,
            "artifact_revision_uids": run.artifact_revision_uids,
            "variant": {
                "name": run.variant.name,
                "artifact_revision_uids": run.variant.artifact_revision_uids,
                "skill_refs": run.variant.skill_refs,
                "workflow_ref": run.variant.workflow_ref,
                "metadata": run.variant.metadata,
            },
        },
        "evidence_refs": {
            "experiment_run_uid": run.run_uid,
            "run_score_run_id": run.score_run_id,
            "plan_revision_uid": evidence.plan_revision_uid,
            "trial_uids": evidence.completed_trials.iter().map(|trial| trial.trial_uid).collect::<Vec<_>>(),
            "trial_score_run_ids": evidence.completed_trials.iter().map(|trial| trial.score_run_id).collect::<Vec<_>>(),
            "session_ids": evidence.completed_trials.iter().filter_map(|trial| trial.session_id).collect::<Vec<_>>(),
            "workflow_run_uids": evidence.completed_trials.iter().filter_map(|trial| trial.workflow_run_uid).collect::<Vec<_>>(),
            "artifact_revision_refs": artifact_revision_refs(run, evidence.completed_trials, evidence.plan_revision_uid, evidence.draft_artifact_revision_uids),
        },
        "trials": evidence.completed_trials.iter().map(trial_evidence_payload).collect::<Vec<_>>(),
        "scores": {
            "run": evidence.run_score_summary.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
            "trial_rollup": evidence.trial_rollup_rows.iter().map(score_row_payload).collect::<Vec<_>>(),
            "trials": evidence.trial_score_summaries.iter().map(trial_score_payload).collect::<Vec<_>>(),
            "scenarios": evidence.scenario_score_summaries.iter().map(scenario_score_payload).collect::<Vec<_>>(),
        },
        "suggested_changes": {
            "draft_artifact_revision_uids": evidence.draft_artifact_revision_uids,
            "note": "No draft artifacts were created because this proposal path has experiment evidence but no existing suggested artifact patch payload to store as a meaningful draft.",
        },
    })
}

fn artifact_revision_refs(
    run: &ExperimentRunRecord,
    completed_trials: &[ExperimentTrialRecord],
    plan_revision_uid: Uuid,
    draft_artifact_revision_uids: &[Uuid],
) -> Value {
    serde_json::json!({
        "plan_revision_uid": plan_revision_uid,
        "run_artifact_revision_uids": run.artifact_revision_uids,
        "variant_artifact_revision_uids": run.variant.artifact_revision_uids,
        "scenario_ids": completed_trials.iter().filter_map(|trial| trial.scenario_id.clone()).collect::<Vec<_>>(),
        "persona_ids": completed_trials.iter().filter_map(|trial| trial.persona_id.clone()).collect::<Vec<_>>(),
        "profile_ids": completed_trials.iter().filter_map(|trial| trial.profile_id.clone()).collect::<Vec<_>>(),
        "data_bundle_ids": completed_trials.iter().flat_map(|trial| trial.data_bundle_ids.iter().cloned()).collect::<Vec<_>>(),
        "trial_artifact_revision_uids": completed_trials.iter().flat_map(|trial| trial.artifact_revision_uids.iter().copied()).collect::<Vec<_>>(),
        "draft_artifact_revision_uids": draft_artifact_revision_uids,
    })
}

fn trial_evidence_payload(trial: &ExperimentTrialRecord) -> Value {
    serde_json::json!({
        "trial_uid": trial.trial_uid,
        "trial_key": trial.trial_key.clone(),
        "status": trial.status.as_str(),
        "target_kind": trial.target_kind.as_str(),
        "variant_key": trial.variant_key.clone(),
        "scenario_id": trial.scenario_id.clone(),
        "persona_id": trial.persona_id.clone(),
        "profile_id": trial.profile_id.clone(),
        "data_bundle_ids": trial.data_bundle_ids.clone(),
        "artifact_revision_uids": trial.artifact_revision_uids.clone(),
        "session_id": trial.session_id,
        "workflow_run_uid": trial.workflow_run_uid,
        "score_run_id": trial.score_run_id,
        "turn_count": trial.turn_count,
        "stop_reason": trial.stop_reason.map(|reason| reason.as_str()),
        "trace_id": trial.trace_id.clone(),
    })
}

fn score_row_payload(row: &ScoreSummaryRow) -> Value {
    serde_json::json!({
        "name": row.name,
        "value_type": row.value_type,
        "n": row.n,
        "mean_or_rate": row.mean_or_rate,
    })
}

fn trial_score_payload(trial: &TrialScoreSummary) -> Value {
    serde_json::json!({
        "trial_uid": trial.trial_uid,
        "trial_key": trial.trial_key.clone(),
        "score_run_id": trial.score_run_id,
        "variant_key": trial.variant_key.clone(),
        "scenario_id": trial.scenario_id.clone(),
        "rows": trial.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
    })
}

fn scenario_score_payload(scenario: &ScenarioScoreSummary) -> Value {
    serde_json::json!({
        "scenario_id": scenario.scenario_id.clone(),
        "rows": scenario.rows.iter().map(score_row_payload).collect::<Vec<_>>(),
    })
}

fn deterministic_candidate_id(
    workspace_id: &WorkspaceId,
    run_uid: Uuid,
    idempotency_key: &str,
) -> Uuid {
    let digest = blake3::hash(
        format!("experiment_learning_proposal:{workspace_id}:{run_uid}:{idempotency_key}")
            .as_bytes(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
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

fn plan_generation_request(
    request: &ExperimentGeneratePlanRequest,
) -> Result<CompletionRequest, HandlerError> {
    let mut completion = CompletionRequest::new(plan_generation_user_prompt(request));
    completion.messages.insert(
        0,
        ContextMessage::system(
            "Generate one canonical MOA behavior-lab experiment_plan artifact document as JSON. \
             Return only the JSON object. Do not start, run, publish, or execute the plan.",
        ),
    );
    completion.model = request.model.as_ref().map(ModelId::new);
    completion.max_output_tokens = Some(4096);
    completion.temperature = Some(0.2);
    completion.response_format = Some(experiment_plan_response_format()?);
    Ok(completion)
}

fn plan_generation_user_prompt(request: &ExperimentGeneratePlanRequest) -> String {
    let refs = if request.artifact_refs.is_empty() {
        "No existing artifact references were supplied.".to_string()
    } else {
        format!(
            "Use these existing artifact references when they fit the requested matrix:\n{}",
            request
                .artifact_refs
                .iter()
                .map(|artifact_ref| format!("- {artifact_ref}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    format!(
        "Create a draft MOA artifact document with api_version `moa.artifact/v1`, kind \
         `experiment_plan`, status `draft`, and definition.type `experiment_plan`.\n\n\
         The document must validate as a draft experiment_plan artifact. Put scenarios, personas, \
         profiles, and optional data_bundles inside definition.spec.simulation as embedded objects \
         with stable `id` fields. Include at least one scenario, persona, profile, target variant, \
         simulator_model, parallelism, trials_per_combination, and budget.max_total_cents. Use \
         stable snake_case or kebab-case names in metadata.name, simulation IDs, and target variant \
         keys.\n\n\
         Plan description:\n{}\n\n{}",
        request.description.trim(),
        refs
    )
}

fn experiment_plan_response_format() -> Result<JsonResponseFormat, HandlerError> {
    Ok(JsonResponseFormat::strict_json_schema(
        "moa_experiment_plan_artifact",
        "A MOA artifact document whose kind and definition type are both experiment_plan.",
        experiment_plan_response_schema(),
    ))
}

fn parse_generated_plan_document(source_text: &str) -> Result<ArtifactDocument, HandlerError> {
    ArtifactDocument::from_json(source_text.trim()).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("generated experiment plan is not valid artifact JSON: {error}"),
        )
        .into()
    })
}

fn require_valid_generated_plan(document: &ArtifactDocument) -> Result<(), HandlerError> {
    if document.kind != ArtifactKind::ExperimentPlan {
        return Err(generated_plan_validation_error(
            "generated artifact kind must be experiment_plan",
            &validate_for_status(document, ArtifactStatus::Draft),
        ));
    }

    let report = validate_for_status(document, ArtifactStatus::Draft);
    if !report.is_ok() {
        return Err(generated_plan_validation_error(
            "generated experiment plan failed draft validation",
            &report,
        ));
    }
    Ok(())
}

fn generated_plan_validation_error(message: &str, report: &ValidationReport) -> HandlerError {
    let report = match serde_json::to_string(report) {
        Ok(report) => report,
        Err(_) => "{}".to_string(),
    };
    TerminalError::new_with_code(400, format!("{message}: {report}")).into()
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

fn parse_trial_status(status: &str) -> Result<ExperimentTrialStatus, HandlerError> {
    ExperimentTrialStatus::from_db(status)
        .ok_or_else(|| bad_request(format!("invalid experiment trial status `{status}`")))
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

fn trial_summary_from_record(
    workspace_id: WorkspaceId,
    trial: &ExperimentTrialRecord,
) -> ExperimentTrialSummary {
    ExperimentTrialSummary {
        workspace_id,
        run_uid: trial.run_uid,
        trial_uid: trial.trial_uid,
        status: trial.status.as_str().to_string(),
        target_kind: trial.target_kind.as_str().to_string(),
        trial_key: trial.trial_key.clone(),
        variant_key: trial.variant_key.clone(),
        scenario_id: trial.scenario_id.clone(),
        score_run_id: trial.score_run_id,
        session_id: trial.session_id,
        workflow_run_uid: trial.workflow_run_uid,
        trace_id: trial.trace_id.clone(),
        stop_reason: trial.stop_reason.map(|reason| reason.as_str().to_string()),
        error: trial.error.clone(),
        turn_count: trial.turn_count,
    }
}

fn experiment_score_summary_row(row: ScoreSummaryRow) -> ExperimentScoreSummaryRow {
    ExperimentScoreSummaryRow {
        name: row.name,
        value_type: row.value_type,
        n: row.n,
        mean_or_rate: row.mean_or_rate,
    }
}

fn experiment_trial_score_summary(row: TrialScoreSummary) -> ExperimentTrialScoreSummary {
    ExperimentTrialScoreSummary {
        trial_uid: row.trial_uid,
        trial_key: row.trial_key,
        score_run_id: row.score_run_id,
        variant_key: row.variant_key,
        scenario_id: row.scenario_id,
        rows: row
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
    }
}

fn experiment_scenario_score_summary(row: ScenarioScoreSummary) -> ExperimentScenarioScoreSummary {
    ExperimentScenarioScoreSummary {
        scenario_id: row.scenario_id,
        rows: row
            .rows
            .into_iter()
            .map(experiment_score_summary_row)
            .collect(),
    }
}

fn experiment_compare_row(row: ScoreCompareRow) -> ExperimentCompareRow {
    ExperimentCompareRow {
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn experiment_scenario_score_delta_row(
    row: ScenarioScoreDeltaRow,
) -> ExperimentScenarioScoreDeltaRow {
    ExperimentScenarioScoreDeltaRow {
        scenario_id: row.scenario_id,
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn experiment_variant_score_delta_row(row: VariantScoreDeltaRow) -> ExperimentVariantScoreDeltaRow {
    ExperimentVariantScoreDeltaRow {
        variant_key: row.variant_key,
        name: row.name,
        base_mean: row.base_mean,
        new_mean: row.new_mean,
        delta: row.delta,
    }
}

fn run_not_found(run_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment run {run_uid} not found")).into()
}

fn trial_not_found(trial_uid: Uuid) -> HandlerError {
    TerminalError::new_with_code(404, format!("experiment trial {trial_uid} not found")).into()
}

fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

fn artifact_doc_error(error: moa_artifacts::Error) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
}

fn plan_expansion_error_to_handler_error(error: PlanExpansionError) -> HandlerError {
    bad_request(error.to_string())
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
