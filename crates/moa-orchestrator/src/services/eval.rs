//! Restate service for hosted eval planning, execution, datasets, replay, and scores.

#[cfg(feature = "internal-eval-runner")]
use std::sync::Arc;
#[cfg(feature = "internal-eval-runner")]
use std::{future::Future, pin::Pin, result::Result as StdResult};

#[cfg(any(feature = "internal-eval-runner", test))]
use chrono::Utc;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::wire::{
    EvalCompareRequest, EvalCompareResponse, EvalCompareRow, EvalDatasetListRequest,
    EvalDatasetListResponse, EvalDatasetRegisterRequest, EvalDatasetRegisterResponse,
    EvalDatasetSummary, EvalPlanRequest, EvalPlanResponse, EvalReplayRequest, EvalReplayResponse,
    EvalRunRequest, EvalRunResponse, EvalRunStatus, EvalRunStatusRequest, EvalRunStatusResponse,
    EvalScoreSummaryRow, EvalScoresRequest, EvalScoresResponse, EvalSuiteListRequest,
    EvalSuiteListResponse, EvalSuiteSummary,
};
#[cfg(feature = "internal-eval-runner")]
use moa_core::{MemoryScope, ScopeContext, ScopedConn};
use moa_core::{MoaConfig, WorkspaceId};
#[cfg(feature = "internal-eval-runner")]
use moa_eval::{
    EngineOptions, EvalEngine, EvaluatorOptions, ReporterOptions, build_evaluators,
    build_reporters, evaluate_run,
};
use moa_eval_core::{AgentConfig, EvalRun as CoreEvalRun, TestSuite, build_eval_plan};
#[cfg(any(feature = "internal-eval-runner", test))]
use moa_eval_core::{EvalResult, ReplayConfig, token_f1};
#[cfg(feature = "internal-eval-runner")]
use moa_eval_core::{EvalStatus, ExpectedOutput, TestCase};
#[cfg(feature = "internal-eval-runner")]
use moa_lineage_core::{LineageEvent, LineageSink};
#[cfg(any(feature = "internal-eval-runner", test))]
use moa_lineage_core::{ScoreRecord, ScoreSource, ScoreTarget, ScoreValue};
#[cfg(feature = "internal-eval-runner")]
use moa_lineage_sink::{MpscSink, MpscSinkConfig};
#[cfg(feature = "internal-eval-runner")]
use moa_scoring::{SCORE_RUN_SOURCE_EVAL_REPLAY, ensure_score_run_parent};
use moa_scoring::{
    ScoreCompare, ScoreCompareRef, ScoreRunRef, ScoreSummary, ScoringError,
    compare_score_runs_for_workspace, score_summaries_for_workspace,
};
use restate_sdk::prelude::*;
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::workflows::eval_run::EvalRunClient;
#[cfg(feature = "internal-eval-runner")]
use crate::workflows::eval_run::EvalRunWorkflowRequest;

#[cfg(feature = "internal-eval-runner")]
type BoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// Restate service surface for hosted eval operations.
#[restate_sdk::service]
#[name = "Eval"]
pub trait Eval {
    /// Plans a hosted eval suite after a workspace member check.
    async fn plan(request: Json<EvalPlanRequest>) -> Result<Json<EvalPlanResponse>, HandlerError>;

    /// Lists supplied hosted eval suite documents after a workspace member check.
    async fn suites_list(
        request: Json<EvalSuiteListRequest>,
    ) -> Result<Json<EvalSuiteListResponse>, HandlerError>;

    /// Runs a hosted eval suite after a workspace member check.
    async fn run(request: Json<EvalRunRequest>) -> Result<Json<EvalRunResponse>, HandlerError>;

    /// Reads a hosted eval run status after a workspace member check.
    async fn run_status(
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError>;

    /// Registers a hosted eval dataset after workspace member and editor checks.
    async fn datasets_register(
        request: Json<EvalDatasetRegisterRequest>,
    ) -> Result<Json<EvalDatasetRegisterResponse>, HandlerError>;

    /// Lists hosted eval datasets after a workspace member check.
    async fn datasets_list(
        request: Json<EvalDatasetListRequest>,
    ) -> Result<Json<EvalDatasetListResponse>, HandlerError>;

    /// Replays a hosted eval dataset after a workspace member check.
    async fn replay(
        request: Json<EvalReplayRequest>,
    ) -> Result<Json<EvalReplayResponse>, HandlerError>;

    /// Reads hosted eval score summaries after a workspace member check.
    async fn scores(
        request: Json<EvalScoresRequest>,
    ) -> Result<Json<EvalScoresResponse>, HandlerError>;

    /// Compares hosted eval score summaries after a workspace member check.
    async fn compare(
        request: Json<EvalCompareRequest>,
    ) -> Result<Json<EvalCompareResponse>, HandlerError>;
}

/// Concrete hosted eval service implementation.
#[derive(Clone, Default)]
pub struct EvalImpl;

impl Eval for EvalImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn plan(
        &self,
        ctx: Context<'_>,
        request: Json<EvalPlanRequest>,
    ) -> Result<Json<EvalPlanResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "plan");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let config = OrchestratorCtx::current_config().as_ref().clone();

        Ok(ctx
            .run(|| async move {
                plan_eval_suite(config, request)
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_plan")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn suites_list(
        &self,
        ctx: Context<'_>,
        request: Json<EvalSuiteListRequest>,
    ) -> Result<Json<EvalSuiteListResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "suites_list");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move {
                suite_list_response(request)
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_suites_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<EvalRunRequest>,
    ) -> Result<Json<EvalRunResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "run");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        #[cfg(not(feature = "internal-eval-runner"))]
        {
            return Err(TerminalError::new_with_code(
                501,
                "hosted eval execution requires the internal-eval-runner feature",
            )
            .into());
        }

        #[cfg(feature = "internal-eval-runner")]
        {
            let acceptance_request = request.clone();
            let response = ctx
                .run(|| async move {
                    let suite = parse_suite_document(
                        acceptance_request
                            .suite_source
                            .as_deref()
                            .unwrap_or("<inline-suite>"),
                        &acceptance_request.suite_document,
                    )
                    .map_err(eval_error_to_handler_error)?;
                    Ok::<_, HandlerError>(Json(accepted_eval_run_response(
                        acceptance_request.workspace_id,
                        Uuid::now_v7(),
                        suite.name,
                    )))
                })
                .name("eval_run_accept")
                .await?
                .into_inner();
            ctx.workflow_client::<EvalRunClient>(response.run_id.to_string())
                .run(Json(EvalRunWorkflowRequest {
                    run_id: response.run_id,
                    request,
                }))
                .send();
            Ok(Json(response))
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run_status(
        &self,
        ctx: Context<'_>,
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "run_status");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let response = ctx
            .workflow_client::<EvalRunClient>(request.run_id.to_string())
            .status(Json(request.clone()))
            .call()
            .await?
            .into_inner();
        verify_run_status_workspace(&request.workspace_id, &response)
            .map_err(eval_error_to_handler_error)?;
        Ok(Json(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn datasets_register(
        &self,
        ctx: Context<'_>,
        request: Json<EvalDatasetRegisterRequest>,
    ) -> Result<Json<EvalDatasetRegisterResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "datasets_register");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                register_dataset_for_workspace(&pool, request)
                    .await
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_datasets_register")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn datasets_list(
        &self,
        ctx: Context<'_>,
        request: Json<EvalDatasetListRequest>,
    ) -> Result<Json<EvalDatasetListResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "datasets_list");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                list_datasets_for_workspace(&pool, request)
                    .await
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_datasets_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn replay(
        &self,
        ctx: Context<'_>,
        request: Json<EvalReplayRequest>,
    ) -> Result<Json<EvalReplayResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "replay");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        #[cfg(not(feature = "internal-eval-runner"))]
        {
            return Err(TerminalError::new_with_code(
                501,
                "hosted eval replay requires the internal-eval-runner feature",
            )
            .into());
        }

        #[cfg(feature = "internal-eval-runner")]
        {
            let runtime = OrchestratorCtx::current();
            let config = runtime.config().as_ref().clone();
            let pool = runtime.graph_pool();

            run_replay_request_isolated(config, pool, request)
                .await
                .map(Json::from)
                .map_err(eval_error_to_handler_error)
        }
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn scores(
        &self,
        ctx: Context<'_>,
        request: Json<EvalScoresRequest>,
    ) -> Result<Json<EvalScoresResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "scores");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                score_summaries_for_workspace(
                    &pool,
                    ScoreRunRef {
                        workspace_id: request.workspace_id,
                        run_id: request.run_id,
                    },
                )
                .await
                .map(eval_scores_response_from_summary)
                .map_err(scoring_error_to_eval_error)
                .map(Json::from)
                .map_err(eval_error_to_handler_error)
            })
            .name("eval_scores")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn compare(
        &self,
        ctx: Context<'_>,
        request: Json<EvalCompareRequest>,
    ) -> Result<Json<EvalCompareResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "compare");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                compare_score_runs_for_workspace(
                    &pool,
                    ScoreCompareRef {
                        workspace_id: request.workspace_id,
                        base_run: request.base_run,
                        new_run: request.new_run,
                    },
                )
                .await
                .map(eval_compare_response_from_scores)
                .map_err(scoring_error_to_eval_error)
                .map(Json::from)
                .map_err(eval_error_to_handler_error)
            })
            .name("eval_compare")
            .await?)
    }
}

/// Error type for hosted eval request parsing, execution, and scoped storage.
#[derive(Debug, thiserror::Error)]
pub enum EvalServiceError {
    /// The submitted eval document was invalid.
    #[error("{document_source}: {message}")]
    InvalidDocument {
        /// Logical source path or URI.
        document_source: String,
        /// Validation or parser message.
        message: String,
    },
    /// An integer value could not be represented on this platform.
    #[error("{field} is too large")]
    IntegerTooLarge {
        /// Request field that overflowed.
        field: &'static str,
    },
    /// A JSON serialization boundary failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// The eval engine failed.
    #[error(transparent)]
    Eval(Box<moa_eval_core::EvalError>),
    /// Database access failed.
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    /// Lineage writer access failed.
    #[error(transparent)]
    Lineage(#[from] moa_lineage_sink::Error),
    /// Hosted eval execution could not be scheduled.
    #[error("{message}")]
    Runtime {
        /// Runtime or task failure message.
        message: String,
    },
    /// A dataset item attempted to target a different workspace.
    #[error(
        "dataset item at line {line} targets workspace {item_workspace_id}, not authorized workspace {request_workspace_id}"
    )]
    DatasetWorkspaceMismatch {
        /// One-indexed JSONL line number.
        line: usize,
        /// Authorized request workspace.
        request_workspace_id: WorkspaceId,
        /// Workspace supplied by the dataset item.
        item_workspace_id: WorkspaceId,
    },
    /// A dataset had no items in the authorized workspace.
    #[error("dataset {dataset_id} has no items in workspace {workspace_id}")]
    EmptyWorkspaceDataset {
        /// Dataset identifier.
        dataset_id: Uuid,
        /// Authorized request workspace.
        workspace_id: WorkspaceId,
    },
    /// A stored run belongs to a different workspace than the request.
    #[error("eval run {run_id} was not found in workspace {request_workspace_id}")]
    RunWorkspaceMismatch {
        /// Hosted eval run identifier.
        run_id: Uuid,
        /// Authorized request workspace.
        request_workspace_id: WorkspaceId,
    },
}

impl From<moa_eval_core::EvalError> for EvalServiceError {
    fn from(error: moa_eval_core::EvalError) -> Self {
        Self::Eval(Box::new(error))
    }
}

/// Dataset item prepared for workspace-scoped registration.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalDatasetItemInsert {
    /// Dataset item identifier.
    pub item_id: Uuid,
    /// Workspace that owns the item.
    pub workspace_id: WorkspaceId,
    /// Stored item scope document.
    pub scope: Value,
    /// Query text to replay.
    pub query: String,
    /// Optional expected answer.
    pub expected_answer: Option<String>,
    /// Optional expected chunk identifiers.
    pub expected_chunk_ids: Vec<Uuid>,
    /// Stored metadata document.
    pub metadata: Value,
}

#[derive(Debug, Deserialize)]
struct JsonlDatasetItem {
    item_id: Option<Uuid>,
    workspace_id: Option<String>,
    scope: Option<Value>,
    query: String,
    expected_answer: Option<String>,
    expected_chunk_ids: Option<Vec<Uuid>>,
    metadata: Option<Value>,
}

/// Parses JSONL dataset items and constrains every item to the authorized workspace.
pub fn parse_dataset_items_for_workspace(
    workspace_id: &WorkspaceId,
    source_uri: Option<&str>,
    jsonl: &str,
) -> Result<Vec<EvalDatasetItemInsert>, EvalServiceError> {
    let source = source_uri.unwrap_or("<inline-jsonl>");
    jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            let parsed: JsonlDatasetItem =
                serde_json::from_str(line).map_err(|error| EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: format!("invalid JSONL item at line {}: {error}", idx + 1),
                })?;
            if parsed.query.trim().is_empty() {
                return Err(EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: format!("dataset item at line {} has an empty query", idx + 1),
                });
            }
            if let Some(item_workspace_id) = parsed.workspace_id.as_deref()
                && item_workspace_id != workspace_id.as_str()
            {
                return Err(EvalServiceError::DatasetWorkspaceMismatch {
                    line: idx + 1,
                    request_workspace_id: workspace_id.clone(),
                    item_workspace_id: WorkspaceId::new(item_workspace_id),
                });
            }
            Ok(EvalDatasetItemInsert {
                item_id: parsed.item_id.unwrap_or_else(Uuid::now_v7),
                workspace_id: workspace_id.clone(),
                scope: parsed.scope.unwrap_or_else(|| serde_json::json!({})),
                query: parsed.query,
                expected_answer: parsed.expected_answer,
                expected_chunk_ids: parsed.expected_chunk_ids.unwrap_or_default(),
                metadata: parsed.metadata.unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect()
}

/// Verifies that a workflow status response is scoped to the requested workspace.
pub fn verify_run_status_workspace(
    request_workspace_id: &WorkspaceId,
    response: &EvalRunStatusResponse,
) -> Result<(), EvalServiceError> {
    if &response.workspace_id != request_workspace_id {
        return Err(EvalServiceError::RunWorkspaceMismatch {
            run_id: response.run_id,
            request_workspace_id: request_workspace_id.clone(),
        });
    }
    Ok(())
}

/// Converts a terminal run response into a status-poll response.
#[must_use]
pub fn status_response_from_run_response(response: &EvalRunResponse) -> EvalRunStatusResponse {
    EvalRunStatusResponse {
        workspace_id: response.workspace_id.clone(),
        run_id: response.run_id,
        status: response.status,
        suite_name: Some(response.suite_name.clone()),
        exit_code: Some(response.exit_code),
        summary: Some(response.summary.clone()),
        results: response.results.clone(),
        error: response.error.clone(),
    }
}

/// Builds a non-terminal accepted run response after starting a hosted workflow.
#[must_use]
pub fn accepted_eval_run_response(
    workspace_id: WorkspaceId,
    run_id: Uuid,
    suite_name: String,
) -> EvalRunResponse {
    EvalRunResponse {
        workspace_id,
        run_id,
        status: EvalRunStatus::Running,
        suite_name,
        exit_code: 2,
        summary: serde_json::json!({}),
        results: Vec::new(),
        error: None,
    }
}

/// Builds a failed terminal run response for errors before case-level results exist.
#[must_use]
pub fn failed_eval_run_response(
    workspace_id: WorkspaceId,
    run_id: Uuid,
    error: impl Into<String>,
) -> EvalRunResponse {
    EvalRunResponse {
        workspace_id,
        run_id,
        status: EvalRunStatus::Failed,
        suite_name: String::new(),
        exit_code: 2,
        summary: serde_json::json!({
            "total_cases": 0,
            "passed": 0,
            "failed": 0,
            "errors": 1,
            "timeouts": 0,
            "total_tokens": 0,
            "total_cost_dollars": 0.0,
            "total_duration_ms": 0
        }),
        results: Vec::new(),
        error: Some(error.into()),
    }
}

/// Runs an already-authorized eval request inside a hosted workflow.
pub async fn execute_eval_run_request(run_id: Uuid, request: EvalRunRequest) -> EvalRunResponse {
    #[cfg(not(feature = "internal-eval-runner"))]
    {
        failed_eval_run_response(
            request.workspace_id,
            run_id,
            "hosted eval execution requires the internal-eval-runner feature",
        )
    }

    #[cfg(feature = "internal-eval-runner")]
    {
        let workspace_id = request.workspace_id.clone();
        match Box::pin(execute_eval_run_request_inner(run_id, request)).await {
            Ok(response) => response,
            Err(error) => failed_eval_run_response(workspace_id, run_id, error.to_string()),
        }
    }
}

/// Runs an already-authorized eval request on an isolated current-thread runtime.
pub async fn execute_eval_run_request_isolated(
    run_id: Uuid,
    request: EvalRunRequest,
) -> EvalRunResponse {
    #[cfg(not(feature = "internal-eval-runner"))]
    {
        failed_eval_run_response(
            request.workspace_id,
            run_id,
            "hosted eval execution requires the internal-eval-runner feature",
        )
    }

    #[cfg(feature = "internal-eval-runner")]
    {
        let workspace_id = request.workspace_id.clone();
        let join = tokio::task::spawn_blocking(move || {
            block_on_current_thread(Box::pin(execute_eval_run_request(run_id, request)))
        })
        .await;
        match join {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => failed_eval_run_response(workspace_id, run_id, error),
            Err(error) => failed_eval_run_response(workspace_id, run_id, error.to_string()),
        }
    }
}

fn plan_eval_suite(
    config: MoaConfig,
    request: EvalPlanRequest,
) -> Result<EvalPlanResponse, EvalServiceError> {
    let suite = parse_suite_document(
        request.suite_source.as_deref().unwrap_or("<inline-suite>"),
        &request.suite_document,
    )?;
    let configs = parse_agent_config_documents(&request.config_sources, &request.config_documents)?;
    let plan = build_eval_plan(&config, &suite, &configs);
    Ok(EvalPlanResponse {
        suite_name: plan.suite_name,
        configs: plan.configs,
        cases: plan.cases,
        total_runs: u64::try_from(plan.total_runs).map_err(|_| {
            EvalServiceError::IntegerTooLarge {
                field: "total_runs",
            }
        })?,
        estimated_min_cost_dollars: plan.estimated_cost_range.0,
        estimated_max_cost_dollars: plan.estimated_cost_range.1,
    })
}

/// Builds eval suite summaries from API-supplied suite documents.
pub fn suite_summaries_from_documents(
    documents: Vec<moa_core::wire::EvalSuiteListDocument>,
) -> Result<Vec<EvalSuiteSummary>, EvalServiceError> {
    documents
        .into_iter()
        .map(|document| {
            let source = document.source;
            let suite = parse_suite_document(
                source.as_deref().unwrap_or("<inline-suite>"),
                &document.body,
            )?;
            Ok(EvalSuiteSummary {
                source,
                name: suite.name,
                cases: u64::try_from(suite.cases.len())
                    .map_err(|_| EvalServiceError::IntegerTooLarge { field: "cases" })?,
                description: suite.description,
                tags: suite.tags,
            })
        })
        .collect()
}

fn suite_list_response(
    request: EvalSuiteListRequest,
) -> Result<EvalSuiteListResponse, EvalServiceError> {
    Ok(EvalSuiteListResponse {
        workspace_id: request.workspace_id,
        suites: suite_summaries_from_documents(request.documents)?,
    })
}

#[cfg(feature = "internal-eval-runner")]
async fn execute_eval_run_request_inner(
    run_id: Uuid,
    request: EvalRunRequest,
) -> Result<EvalRunResponse, EvalServiceError> {
    let suite = parse_suite_document(
        request.suite_source.as_deref().unwrap_or("<inline-suite>"),
        &request.suite_document,
    )?;
    let configs = parse_agent_config_documents(&request.config_sources, &request.config_documents)?;
    let evaluators = build_evaluators(
        &request.evaluators,
        &EvaluatorOptions {
            max_cost_dollars: request.max_cost_dollars,
            max_latency_ms: request.max_latency_ms,
            max_tokens: option_u64_to_usize(request.max_tokens, "max_tokens")?,
            max_tool_calls: option_u64_to_usize(request.max_tool_calls, "max_tool_calls")?,
            max_turns: option_u64_to_usize(request.max_turns, "max_turns")?,
        },
    )?;
    let engine = EvalEngine::new(
        OrchestratorCtx::current_config().as_ref().clone(),
        EngineOptions {
            parallel: usize::try_from(request.parallel)
                .map_err(|_| EvalServiceError::IntegerTooLarge { field: "parallel" })?,
            ..EngineOptions::default()
        },
    )?;
    let mut run = engine.run_suite(&suite, &configs).await?;
    evaluate_run(&suite, &mut run, &evaluators).await?;
    let exit_code = eval_ci_exit_code(request.ci, &run.results);
    let report_artifacts =
        hosted_eval_report_artifacts(&suite, &configs, &run, &request.reports, request.verbose)
            .await?;
    let mut summary = serde_json::to_value(&run.summary)?;
    if let Some(report_artifacts) = report_artifacts {
        attach_summary_field(&mut summary, "reports", report_artifacts);
    }
    Ok(EvalRunResponse {
        workspace_id: request.workspace_id,
        run_id,
        status: EvalRunStatus::Completed,
        suite_name: run.suite_name,
        exit_code,
        summary,
        results: run
            .results
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?,
        error: None,
    })
}

/// Builds hosted eval report artifacts and emits configured external report sinks.
pub async fn hosted_eval_report_artifacts(
    suite: &TestSuite,
    configs: &[AgentConfig],
    run: &CoreEvalRun,
    specs: &[String],
    verbose: bool,
) -> Result<Option<Value>, EvalServiceError> {
    if specs.is_empty() {
        return Ok(None);
    }

    let mut terminal_reports = Vec::new();
    let mut json_reports = Vec::new();
    for spec in specs {
        if spec == "terminal" {
            terminal_reports.push(render_hosted_terminal_report(suite, configs, run, verbose));
        } else if let Some(target) = spec.strip_prefix("json:") {
            json_reports.push(serde_json::json!({
                "target": target,
                "document": {
                    "suite": suite,
                    "configs": configs,
                    "run": run,
                }
            }));
        } else if spec == "langfuse" {
            #[cfg(not(feature = "internal-eval-runner"))]
            {
                return Err(EvalServiceError::Runtime {
                    message: "langfuse reports require the internal-eval-runner feature"
                        .to_string(),
                });
            }
            #[cfg(feature = "internal-eval-runner")]
            {
                let reporters = build_reporters(
                    std::slice::from_ref(spec),
                    &ReporterOptions {
                        verbose,
                        color: false,
                        json_pretty: true,
                    },
                )?;
                for reporter in reporters {
                    reporter.report(suite, configs, run).await?;
                }
            }
        } else {
            return Err(moa_eval_core::EvalError::InvalidConfig(format!(
                "unknown report target '{spec}'"
            ))
            .into());
        }
    }

    Ok(Some(serde_json::json!({
        "terminal": terminal_reports,
        "json": json_reports,
    })))
}

fn render_hosted_terminal_report(
    suite: &TestSuite,
    configs: &[AgentConfig],
    run: &CoreEvalRun,
    verbose: bool,
) -> String {
    let mut output = format!(
        "Suite: {}\nConfigs: {}\nCases: {}\nPassed: {}\nFailed: {}\nErrors: {}\nTimeouts: {}\n",
        suite.name,
        configs
            .iter()
            .map(|config| config.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        suite.cases.len(),
        run.summary.passed,
        run.summary.failed,
        run.summary.errors,
        run.summary.timeouts
    );
    if verbose {
        for result in &run.results {
            output.push_str(&format!(
                "{} / {}: {:?}\n",
                result.agent_config, result.test_case, result.status
            ));
        }
    }
    output
}

#[cfg(feature = "internal-eval-runner")]
fn attach_summary_field(summary: &mut Value, field: &str, value: Value) {
    if let Some(object) = summary.as_object_mut() {
        object.insert(field.to_string(), value);
    }
}

async fn register_dataset_for_workspace(
    pool: &PgPool,
    request: EvalDatasetRegisterRequest,
) -> Result<EvalDatasetRegisterResponse, EvalServiceError> {
    let items = parse_dataset_items_for_workspace(
        &request.workspace_id,
        request.source_uri.as_deref(),
        &request.jsonl,
    )?;
    if items.is_empty() {
        return Err(EvalServiceError::InvalidDocument {
            document_source: request
                .source_uri
                .clone()
                .unwrap_or_else(|| "<inline-jsonl>".to_string()),
            message: "dataset contains no items for the authorized workspace".to_string(),
        });
    }

    let mut tx = pool.begin().await?;
    let proposed_dataset_id = Uuid::now_v7();
    let dataset_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO analytics.eval_datasets (dataset_id, name, source_path)
        VALUES ($1, $2, $3)
        ON CONFLICT (name) DO UPDATE
        SET source_path = EXCLUDED.source_path
        RETURNING dataset_id
        "#,
    )
    .bind(proposed_dataset_id)
    .bind(&request.name)
    .bind(request.source_uri.as_deref())
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "DELETE FROM analytics.eval_dataset_items WHERE dataset_id = $1 AND workspace_id = $2",
    )
    .bind(dataset_id)
    .bind(request.workspace_id.as_str())
    .execute(&mut *tx)
    .await?;

    let mut item_insert = QueryBuilder::<Postgres>::new(
        r#"
        INSERT INTO analytics.eval_dataset_items (
            item_id,
            dataset_id,
            workspace_id,
            scope,
            query,
            expected_answer,
            expected_chunk_ids,
            metadata
        )
        "#,
    );
    item_insert.push_values(&items, |mut row, item| {
        row.push_bind(item.item_id)
            .push_bind(dataset_id)
            .push_bind(item.workspace_id.as_str())
            .push_bind(sqlx::types::Json(&item.scope))
            .push_bind(&item.query)
            .push_bind(item.expected_answer.as_deref())
            .push_bind(&item.expected_chunk_ids)
            .push_bind(sqlx::types::Json(&item.metadata));
    });
    item_insert.build().execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(EvalDatasetRegisterResponse {
        workspace_id: request.workspace_id,
        dataset_id,
        name: request.name,
        items: u64::try_from(items.len())
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
    })
}

async fn list_datasets_for_workspace(
    pool: &PgPool,
    request: EvalDatasetListRequest,
) -> Result<EvalDatasetListResponse, EvalServiceError> {
    let rows = sqlx::query(
        r#"
        SELECT d.dataset_id, d.name, d.source_path, COUNT(i.item_id)::BIGINT AS items
        FROM analytics.eval_datasets d
        JOIN analytics.eval_dataset_items i
          ON i.dataset_id = d.dataset_id AND i.workspace_id = $1
        GROUP BY d.dataset_id, d.name, d.source_path, d.created_at
        ORDER BY d.created_at DESC
        "#,
    )
    .bind(request.workspace_id.as_str())
    .fetch_all(pool)
    .await?;

    let mut datasets = Vec::with_capacity(rows.len());
    for row in rows {
        let items: i64 = row.try_get("items")?;
        datasets.push(EvalDatasetSummary {
            workspace_id: request.workspace_id.clone(),
            dataset_id: row.try_get("dataset_id")?,
            name: row.try_get("name")?,
            items: u64::try_from(items)
                .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
            source_uri: row.try_get("source_path")?,
        });
    }
    Ok(EvalDatasetListResponse {
        workspace_id: request.workspace_id,
        datasets,
    })
}

#[cfg(feature = "internal-eval-runner")]
async fn replay_dataset_for_workspace(
    config: MoaConfig,
    pool: PgPool,
    request: EvalReplayRequest,
) -> Result<EvalReplayResponse, EvalServiceError> {
    let run_id = request.run_id.unwrap_or_else(Uuid::now_v7);
    let limit = option_u64_to_usize(request.limit, "limit")?;
    let replay_config = ReplayConfig {
        dataset_id: request.dataset_id,
        run_id,
        model_override: request.model.clone(),
        embedder_override: request.embedder.clone(),
        limit,
    };
    let items = load_dataset_items_for_workspace(
        &pool,
        &request.workspace_id,
        request.dataset_id,
        replay_config.limit,
    )
    .await?;
    if items.is_empty() {
        return Err(EvalServiceError::EmptyWorkspaceDataset {
            dataset_id: request.dataset_id,
            workspace_id: request.workspace_id,
        });
    }
    ensure_eval_replay_score_run_parent(&pool, &request.workspace_id, run_id).await?;

    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        pool.clone(),
    )
    .await?;
    let report = Box::pin(replay_items_live(
        config,
        Arc::new(sink) as Arc<dyn LineageSink>,
        replay_config,
        items,
    ))
    .await?;
    writer.shutdown().await?;
    Ok(EvalReplayResponse {
        workspace_id: request.workspace_id,
        run_id: report.run_id,
        dataset_id: report.dataset_id,
        items: u64::try_from(report.items)
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
        scores: u64::try_from(report.scores)
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "scores" })?,
    })
}

#[cfg(feature = "internal-eval-runner")]
async fn ensure_eval_replay_score_run_parent(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    run_id: Uuid,
) -> Result<(), EvalServiceError> {
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let mut conn = ScopedConn::begin(pool, &ScopeContext::from(scope.clone()))
        .await
        .map_err(|error| EvalServiceError::Runtime {
            message: error.to_string(),
        })?;
    ensure_score_run_parent(conn.as_mut(), &scope, run_id, SCORE_RUN_SOURCE_EVAL_REPLAY)
        .await
        .map_err(scoring_error_to_eval_error)?;
    conn.commit()
        .await
        .map_err(|error| EvalServiceError::Runtime {
            message: error.to_string(),
        })?;
    Ok(())
}

#[cfg(feature = "internal-eval-runner")]
async fn run_replay_request_isolated(
    config: MoaConfig,
    pool: PgPool,
    request: EvalReplayRequest,
) -> Result<EvalReplayResponse, EvalServiceError> {
    tokio::task::spawn_blocking(move || {
        block_on_current_thread(Box::pin(replay_dataset_for_workspace(
            config, pool, request,
        )))
    })
    .await
    .map_err(|error| EvalServiceError::Runtime {
        message: error.to_string(),
    })?
    .map_err(|message| EvalServiceError::Runtime { message })?
}

#[cfg(any(feature = "internal-eval-runner", test))]
#[derive(Clone, Debug)]
struct ScopedDatasetItem {
    item_id: Uuid,
    workspace_id: WorkspaceId,
    #[cfg(feature = "internal-eval-runner")]
    query: String,
    expected_answer: Option<String>,
    expected_chunk_ids: Vec<Uuid>,
}

#[cfg(feature = "internal-eval-runner")]
#[derive(Clone, Debug)]
struct ScopedReplayReport {
    run_id: Uuid,
    dataset_id: Uuid,
    items: usize,
    scores: usize,
}

#[cfg(feature = "internal-eval-runner")]
async fn load_dataset_items_for_workspace(
    pool: &PgPool,
    workspace_id: &WorkspaceId,
    dataset_id: Uuid,
    limit: Option<usize>,
) -> Result<Vec<ScopedDatasetItem>, EvalServiceError> {
    let limit = i64::try_from(limit.unwrap_or(1000))
        .map_err(|_| EvalServiceError::IntegerTooLarge { field: "limit" })?;
    let rows = sqlx::query(
        r#"
        SELECT item_id, workspace_id, query, expected_answer, expected_chunk_ids
        FROM analytics.eval_dataset_items
        WHERE dataset_id = $1 AND workspace_id = $2
        ORDER BY created_at ASC, item_id ASC
        LIMIT $3
        "#,
    )
    .bind(dataset_id)
    .bind(workspace_id.as_str())
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let row_workspace_id: String = row.try_get("workspace_id")?;
            Ok(ScopedDatasetItem {
                item_id: row.try_get("item_id")?,
                workspace_id: WorkspaceId::new(row_workspace_id),
                query: row.try_get("query")?,
                expected_answer: row.try_get("expected_answer")?,
                expected_chunk_ids: row.try_get("expected_chunk_ids")?,
            })
        })
        .collect()
}

#[cfg(feature = "internal-eval-runner")]
async fn replay_items_live(
    config: MoaConfig,
    sink: Arc<dyn LineageSink>,
    replay_config: ReplayConfig,
    items: Vec<ScopedDatasetItem>,
) -> Result<ScopedReplayReport, EvalServiceError> {
    let cases = items
        .iter()
        .map(|item| TestCase {
            name: item.item_id.to_string(),
            input: item.query.clone(),
            expected_output: item.expected_answer.as_ref().map(|answer| ExpectedOutput {
                exact: Some(answer.clone()),
                ..ExpectedOutput::default()
            }),
            ..TestCase::default()
        })
        .collect::<Vec<_>>();
    let suite = TestSuite {
        name: format!("replay-{}", replay_config.dataset_id),
        cases,
        default_timeout_seconds: 300,
        ..TestSuite::default()
    };
    let agent_config = AgentConfig {
        name: "replay".to_string(),
        model: replay_config.model_override.clone(),
        ..AgentConfig::default()
    };
    let engine = EvalEngine::new(
        config,
        EngineOptions {
            parallel: 1,
            ..EngineOptions::default()
        },
    )?;
    let run = engine.run_suite(&suite, &[agent_config]).await?;
    let evaluator = replay_evaluator_name(&replay_config);
    let mut report = ScopedReplayReport {
        run_id: replay_config.run_id,
        dataset_id: replay_config.dataset_id,
        items: 0,
        scores: 0,
    };

    for (item, result) in items.iter().zip(run.results.iter()) {
        let records = replay_score_records_for_item(item, result, &replay_config, &evaluator);
        report.scores += records.len();
        for record in records {
            sink.record(LineageEvent::Eval(record));
        }
        report.items += 1;
    }

    Ok(report)
}

#[cfg(any(feature = "internal-eval-runner", test))]
fn replay_score_records_for_item(
    item: &ScopedDatasetItem,
    result: &EvalResult,
    replay_config: &ReplayConfig,
    evaluator: &str,
) -> Vec<ScoreRecord> {
    let mut capacity = if item.expected_answer.is_some() { 1 } else { 0 };
    if !item.expected_chunk_ids.is_empty() {
        capacity += 4;
    }
    let mut records = Vec::with_capacity(capacity);

    if let Some(expected) = &item.expected_answer {
        let actual = result.response.as_deref().unwrap_or_default();
        let score = token_f1(actual, expected);
        records.push(dataset_run_item_score_record(
            item,
            replay_config,
            evaluator,
            "answer_f1",
            ScoreValue::Numeric(score),
            result.error.clone(),
        ));
    }

    if !item.expected_chunk_ids.is_empty() {
        // Replay currently lacks turn-level retrieval lineage, so expected chunks
        // get the explicit zero-recall fallback until lineage is available here.
        for (name, value) in [
            ("retrieval.recall_at_4", ScoreValue::Numeric(0.0)),
            ("retrieval.mrr", ScoreValue::Numeric(0.0)),
            ("retrieval.ndcg_at_4", ScoreValue::Numeric(0.0)),
            ("retrieval.zero_recall", ScoreValue::Boolean(true)),
        ] {
            records.push(dataset_run_item_score_record(
                item,
                replay_config,
                evaluator,
                name,
                value,
                result.error.clone(),
            ));
        }
    }

    records
}

#[cfg(any(feature = "internal-eval-runner", test))]
fn dataset_run_item_score_record(
    item: &ScopedDatasetItem,
    replay_config: &ReplayConfig,
    evaluator: &str,
    name: &str,
    value: ScoreValue,
    comment: Option<String>,
) -> ScoreRecord {
    ScoreRecord {
        score_id: Uuid::now_v7(),
        ts: Utc::now(),
        target: ScoreTarget::DatasetRunItem {
            run_id: replay_config.run_id,
            item_id: item.item_id,
        },
        workspace_id: item.workspace_id.clone(),
        user_id: None,
        name: name.to_string(),
        value,
        source: ScoreSource::OfflineReplay,
        model_or_evaluator: evaluator.to_string(),
        run_id: Some(replay_config.run_id),
        dataset_id: Some(replay_config.dataset_id),
        comment,
    }
}

fn parse_suite_document(source: &str, document: &str) -> Result<TestSuite, EvalServiceError> {
    let suite: TestSuite =
        toml::from_str(document).map_err(|error| EvalServiceError::InvalidDocument {
            document_source: source.to_string(),
            message: error.to_string(),
        })?;
    if suite.name.trim().is_empty() {
        return Err(EvalServiceError::InvalidDocument {
            document_source: source.to_string(),
            message: "suite is missing [suite].name".to_string(),
        });
    }
    Ok(suite)
}

fn parse_agent_config_documents(
    sources: &[String],
    documents: &[String],
) -> Result<Vec<AgentConfig>, EvalServiceError> {
    documents
        .iter()
        .enumerate()
        .map(|(index, document)| {
            let source = sources
                .get(index)
                .map(String::as_str)
                .unwrap_or("<inline-config>");
            let config: AgentConfig =
                toml::from_str(document).map_err(|error| EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: error.to_string(),
                })?;
            if config.name.trim().is_empty() {
                return Err(EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: "agent config is missing [agent].name".to_string(),
                });
            }
            Ok(config)
        })
        .collect()
}

#[cfg(feature = "internal-eval-runner")]
fn option_u64_to_usize(
    value: Option<u64>,
    field: &'static str,
) -> Result<Option<usize>, EvalServiceError> {
    value
        .map(|value| {
            usize::try_from(value).map_err(|_| EvalServiceError::IntegerTooLarge { field })
        })
        .transpose()
}

#[cfg(feature = "internal-eval-runner")]
fn eval_ci_exit_code(ci: bool, results: &[EvalResult]) -> i32 {
    if !ci {
        return 0;
    }
    if results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Error | EvalStatus::Timeout))
    {
        return 2;
    }
    if results
        .iter()
        .any(|result| matches!(result.status, EvalStatus::Failed))
    {
        return 1;
    }
    0
}

#[cfg(feature = "internal-eval-runner")]
fn replay_evaluator_name(cfg: &ReplayConfig) -> String {
    match (&cfg.model_override, &cfg.embedder_override) {
        (Some(model), Some(embedder)) => format!("replay-f1:{model}:{embedder}"),
        (Some(model), None) => format!("replay-f1:{model}"),
        (None, Some(embedder)) => format!("replay-f1:{embedder}"),
        (None, None) => "f1-overlap".to_string(),
    }
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<(), HandlerError> {
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
    .map_err(translate_authz_error)
}

fn eval_error_to_handler_error(error: EvalServiceError) -> HandlerError {
    match error {
        EvalServiceError::InvalidDocument { .. }
        | EvalServiceError::DatasetWorkspaceMismatch { .. }
        | EvalServiceError::IntegerTooLarge { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        EvalServiceError::EmptyWorkspaceDataset { .. }
        | EvalServiceError::RunWorkspaceMismatch { .. } => {
            TerminalError::new_with_code(404, error.to_string()).into()
        }
        EvalServiceError::Json(_)
        | EvalServiceError::Eval(_)
        | EvalServiceError::Sql(_)
        | EvalServiceError::Lineage(_)
        | EvalServiceError::Runtime { .. } => TerminalError::new(error.to_string()).into(),
    }
}

fn eval_scores_response_from_summary(summary: ScoreSummary) -> EvalScoresResponse {
    EvalScoresResponse {
        workspace_id: summary.workspace_id,
        run_id: summary.run_id,
        rows: summary
            .rows
            .into_iter()
            .map(|row| EvalScoreSummaryRow {
                name: row.name,
                value_type: row.value_type,
                n: row.n,
                mean_or_rate: row.mean_or_rate,
            })
            .collect(),
    }
}

fn eval_compare_response_from_scores(compare: ScoreCompare) -> EvalCompareResponse {
    EvalCompareResponse {
        workspace_id: compare.workspace_id,
        base_run: compare.base_run,
        new_run: compare.new_run,
        rows: compare
            .rows
            .into_iter()
            .map(|row| EvalCompareRow {
                name: row.name,
                base_mean: row.base_mean,
                new_mean: row.new_mean,
                delta: row.delta,
            })
            .collect(),
    }
}

fn scoring_error_to_eval_error(error: ScoringError) -> EvalServiceError {
    match error {
        ScoringError::IntegerTooLarge { field } => EvalServiceError::IntegerTooLarge { field },
        ScoringError::Sql(error) => EvalServiceError::Sql(error),
        ScoringError::ScoreRunMismatch { .. } => EvalServiceError::Runtime {
            message: error.to_string(),
        },
    }
}

#[cfg(feature = "internal-eval-runner")]
fn block_on_current_thread<T>(future: BoxFuture<T>) -> StdResult<T, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    Ok(runtime.block_on(future))
}

#[cfg(test)]
mod tests {
    //! Unit coverage for hosted eval replay score-row construction.

    use super::*;

    #[test]
    fn replay_score_records_emit_retrieval_fallback_for_expected_chunks() {
        // Pins: dataset items with expected chunks emit answer_f1 plus the four retrieval fallback scores.
        let run_id = uuid("11111111-1111-1111-1111-111111111111");
        let dataset_id = uuid("22222222-2222-2222-2222-222222222222");
        let item_id = uuid("33333333-3333-3333-3333-333333333333");
        let item = ScopedDatasetItem {
            item_id,
            workspace_id: WorkspaceId::new("workspace-a"),
            #[cfg(feature = "internal-eval-runner")]
            query: "alpha?".to_string(),
            expected_answer: Some("alpha beta".to_string()),
            expected_chunk_ids: vec![
                uuid("44444444-4444-4444-4444-444444444444"),
                uuid("55555555-5555-5555-5555-555555555555"),
            ],
        };
        let result = EvalResult {
            response: Some("alpha beta".to_string()),
            ..EvalResult::default()
        };
        let replay_config = replay_config(run_id, dataset_id);

        let records =
            replay_score_records_for_item(&item, &result, &replay_config, "replay-f1:model");

        assert_eq!(records.len(), 5);
        assert_eq!(
            records
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "answer_f1",
                "retrieval.recall_at_4",
                "retrieval.mrr",
                "retrieval.ndcg_at_4",
                "retrieval.zero_recall",
            ]
        );
        for record in &records {
            assert_dataset_run_item_target(record, run_id, item_id);
            assert_eq!(record.workspace_id, WorkspaceId::new("workspace-a"));
            assert_eq!(record.run_id, Some(run_id));
            assert_eq!(record.dataset_id, Some(dataset_id));
            assert_eq!(record.source, ScoreSource::OfflineReplay);
            assert_eq!(record.model_or_evaluator, "replay-f1:model");
        }
        assert_numeric_score(&records[0], 1.0);
        assert_numeric_score(&records[1], 0.0);
        assert_numeric_score(&records[2], 0.0);
        assert_numeric_score(&records[3], 0.0);
        assert_boolean_score(&records[4], true);
    }

    #[test]
    fn replay_score_records_skip_retrieval_scores_without_expected_chunks() {
        // Pins: answer-only dataset items do not gain retrieval scores when no expected chunks are present.
        let run_id = uuid("11111111-1111-1111-1111-111111111111");
        let dataset_id = uuid("22222222-2222-2222-2222-222222222222");
        let item_id = uuid("33333333-3333-3333-3333-333333333333");
        let item = ScopedDatasetItem {
            item_id,
            workspace_id: WorkspaceId::new("workspace-a"),
            #[cfg(feature = "internal-eval-runner")]
            query: "alpha?".to_string(),
            expected_answer: Some("alpha beta".to_string()),
            expected_chunk_ids: Vec::new(),
        };
        let result = EvalResult {
            response: Some("alpha beta".to_string()),
            ..EvalResult::default()
        };
        let replay_config = replay_config(run_id, dataset_id);

        let records =
            replay_score_records_for_item(&item, &result, &replay_config, "replay-f1:model");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "answer_f1");
        assert!(
            records
                .iter()
                .all(|record| !record.name.starts_with("retrieval."))
        );
        assert_dataset_run_item_target(&records[0], run_id, item_id);
        assert_numeric_score(&records[0], 1.0);
    }

    fn replay_config(run_id: Uuid, dataset_id: Uuid) -> ReplayConfig {
        ReplayConfig {
            dataset_id,
            run_id,
            model_override: Some("model".to_string()),
            embedder_override: None,
            limit: None,
        }
    }

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).expect("fixture UUID should parse")
    }

    fn assert_dataset_run_item_target(record: &ScoreRecord, run_id: Uuid, item_id: Uuid) {
        match &record.target {
            ScoreTarget::DatasetRunItem {
                run_id: actual_run_id,
                item_id: actual_item_id,
            } => {
                assert_eq!(*actual_run_id, run_id);
                assert_eq!(*actual_item_id, item_id);
            }
            other => panic!("expected dataset run item target, got {other:?}"),
        }
    }

    fn assert_numeric_score(record: &ScoreRecord, expected: f64) {
        match &record.value {
            ScoreValue::Numeric(actual) => assert_eq!(*actual, expected),
            other => panic!("expected numeric score, got {other:?}"),
        }
    }

    fn assert_boolean_score(record: &ScoreRecord, expected: bool) {
        match &record.value {
            ScoreValue::Boolean(actual) => assert_eq!(*actual, expected),
            other => panic!("expected boolean score, got {other:?}"),
        }
    }
}
