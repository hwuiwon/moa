//! Restate service for hosted eval planning, execution, datasets, replay, and scores.

pub mod repository;

use std::sync::Arc;
use std::{future::Future, pin::Pin, result::Result as StdResult};

use chrono::Utc;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::memory::RlsContext;
use moa_core::wire::eval::{
    EvalCompareRequest, EvalCompareResponse, EvalCompareRow, EvalDatasetListRequest,
    EvalDatasetListResponse, EvalDatasetRegisterRequest, EvalDatasetRegisterResponse,
    EvalPlanRequest, EvalPlanResponse, EvalReplayRequest, EvalReplayResponse, EvalRunRequest,
    EvalRunResponse, EvalRunStatus, EvalRunStatusRequest, EvalRunStatusResponse,
    EvalScoreSummaryRow, EvalScoresRequest, EvalScoresResponse, EvalSuiteListRequest,
    EvalSuiteListResponse, EvalSuiteSummary,
};
use moa_core::{
    config::MoaConfig, types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use moa_eval::{EvalEngine, build_eval_plan};
use moa_eval_core::{AgentConfig, EvalRun as CoreEvalRun, TestSuite};
use moa_eval_core::{EngineOptions, EvaluatorOptions, build_evaluators, evaluate_run};
use moa_eval_core::{EvalResult, ReplayConfig, token_f1};
use moa_eval_core::{EvalStatus, ExpectedOutput, TestCase};
use moa_lineage_core::LineageEvent;
use moa_lineage_core::{ScoreRecord, ScoreSource, ScoreTarget, ScoreValue};
use moa_lineage_sink::{MpscSink, MpscSinkConfig};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_scoring::{SCORE_RUN_SOURCE_EVAL_REPLAY, ensure_score_run_parent};
use moa_scoring::{
    ScoreCompare, ScoreCompareRef, ScoreRunRef, ScoreSummary, ScoringError,
    compare_score_runs_for_tenant, score_summaries_for_tenant,
};
use repository::ScopedDatasetItem;
use repository::load_dataset_items_for_tenant;
use repository::{list_datasets_for_tenant, register_dataset_for_tenant};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::handlers::authz_shim::authorize_tenant_operator_or_admin;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;
const INTERNAL_EVAL_DISPATCH_TOKEN_HASH_FIELD: &str = "_moa_internal_dispatch_token_sha256";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunExecutionRequest {
    /// Server-assigned hosted eval run identifier.
    pub run_id: Uuid,
    /// Original client eval run request accepted by `Eval/run`.
    pub request: EvalRunRequest,
    /// Opaque server-issued token created when `Eval/run` accepts the request.
    pub dispatch_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AcceptedEvalRunDispatch {
    response: EvalRunResponse,
    dispatch_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredEvalRunExecutionState {
    status: EvalRunStatus,
    response: Option<EvalRunResponse>,
    dispatch_token_hash: Option<String>,
}

/// Restate service surface for hosted eval operations.
#[restate_sdk::service]
#[name = "Eval"]
pub trait Eval {
    /// Plans a hosted eval suite after a tenant operator/admin check.
    async fn plan(request: Json<EvalPlanRequest>) -> Result<Json<EvalPlanResponse>, HandlerError>;

    /// Lists supplied hosted eval suite documents after a tenant operator/admin check.
    async fn suites_list(
        request: Json<EvalSuiteListRequest>,
    ) -> Result<Json<EvalSuiteListResponse>, HandlerError>;

    /// Runs a hosted eval suite after a tenant operator/admin check.
    async fn run(request: Json<EvalRunRequest>) -> Result<Json<EvalRunResponse>, HandlerError>;

    /// Executes an already accepted hosted eval run.
    async fn execute_run(
        request: Json<EvalRunExecutionRequest>,
    ) -> Result<Json<EvalRunResponse>, HandlerError>;

    /// Reads a hosted eval run status after a tenant operator/admin check.
    async fn run_status(
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError>;

    /// Registers a hosted eval dataset after tenant operator/admin checks.
    async fn datasets_register(
        request: Json<EvalDatasetRegisterRequest>,
    ) -> Result<Json<EvalDatasetRegisterResponse>, HandlerError>;

    /// Lists hosted eval datasets after a tenant operator/admin check.
    async fn datasets_list(
        request: Json<EvalDatasetListRequest>,
    ) -> Result<Json<EvalDatasetListResponse>, HandlerError>;

    /// Replays a hosted eval dataset after a tenant operator/admin check.
    async fn replay(
        request: Json<EvalReplayRequest>,
    ) -> Result<Json<EvalReplayResponse>, HandlerError>;

    /// Reads hosted eval score summaries after a tenant operator/admin check.
    async fn scores(
        request: Json<EvalScoresRequest>,
    ) -> Result<Json<EvalScoresResponse>, HandlerError>;

    /// Compares hosted eval score summaries after a tenant operator/admin check.
    async fn compare(
        request: Json<EvalCompareRequest>,
    ) -> Result<Json<EvalCompareResponse>, HandlerError>;
}

/// Concrete hosted eval service implementation.
#[derive(Clone)]
pub struct EvalImpl {
    pool: PgPool,
    config: Arc<MoaConfig>,
}

impl EvalImpl {
    /// Creates the hosted eval service with its storage and runtime configuration.
    #[must_use]
    pub fn new(pool: PgPool, config: Arc<MoaConfig>) -> Self {
        Self { pool, config }
    }
}

impl Eval for EvalImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn plan(
        &self,
        ctx: Context<'_>,
        request: Json<EvalPlanRequest>,
    ) -> Result<Json<EvalPlanResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "plan");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let config = self.config.as_ref().clone();

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
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;

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
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;

        let pool = self.pool.clone();
        let acceptance_request = request.clone();
        let accepted = ctx
            .run(|| async move {
                let suite = parse_suite_document(
                    acceptance_request
                        .suite_source
                        .as_deref()
                        .unwrap_or("<inline-suite>"),
                    &acceptance_request.suite_document,
                )
                .map_err(eval_error_to_handler_error)?;
                let response = accepted_eval_run_response(
                    acceptance_request.tenant_id,
                    Uuid::now_v7(),
                    suite.name,
                );
                let dispatch_token = generate_internal_eval_dispatch_token();
                persist_accepted_eval_run(&pool, &acceptance_request, &response, &dispatch_token)
                    .await
                    .map_err(eval_error_to_handler_error)?;
                Ok::<_, HandlerError>(Json(AcceptedEvalRunDispatch {
                    response,
                    dispatch_token,
                }))
            })
            .name("eval_run_accept")
            .await?
            .into_inner();
        ctx.service_client::<EvalClient>()
            .execute_run(Json(EvalRunExecutionRequest {
                run_id: accepted.response.run_id,
                request,
                dispatch_token: accepted.dispatch_token,
            }))
            .send();
        Ok(Json(accepted.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal worker entrypoint; the dispatch-token guard below must pass before run data is returned or mutated.
    async fn execute_run(
        &self,
        ctx: Context<'_>,
        request: Json<EvalRunExecutionRequest>,
    ) -> Result<Json<EvalRunResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "execute_run");
        let request = request.into_inner();
        let run_id = request.run_id;
        let tenant_id = request.request.tenant_id;
        let pool = self.pool.clone();
        let existing = ctx
            .run(move || async move {
                load_eval_run_execution_state(&pool, tenant_id, run_id)
                    .await
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_run_load")
            .await?
            .into_inner();

        verify_internal_eval_dispatch_token(
            run_id,
            &request.dispatch_token,
            existing.dispatch_token_hash.as_deref(),
        )
        .map_err(eval_error_to_handler_error)?;

        if let Some(response) = existing.response {
            if !eval_run_status_is_terminal(response.status) {
                return Err(TerminalError::new_with_code(
                    409,
                    "persisted eval run response is not terminal",
                )
                .into());
            }
            return Ok(Json(response));
        }
        if eval_run_status_is_terminal(existing.status) {
            return Ok(Json(failed_eval_run_response(
                tenant_id,
                run_id,
                "terminal eval run is missing persisted response",
            )));
        }

        let pool = self.pool.clone();
        ctx.run(move || async move {
            mark_eval_run_running(&pool, tenant_id, run_id)
                .await
                .map_err(eval_error_to_handler_error)
        })
        .name("eval_run_mark_running")
        .await?;

        let pool = self.pool.clone();
        let config = self.config.as_ref().clone();
        Ok(ctx
            .run(move || async move {
                let response = normalize_eval_run_response(
                    tenant_id,
                    run_id,
                    execute_eval_run_request_isolated(config, run_id, request.request).await,
                );
                persist_terminal_eval_run_response(&pool, &response)
                    .await
                    .map_err(eval_error_to_handler_error)?;
                Ok::<_, HandlerError>(Json(response))
            })
            .name("eval_run_execute")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run_status(
        &self,
        ctx: Context<'_>,
        request: Json<EvalRunStatusRequest>,
    ) -> Result<Json<EvalRunStatusResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "run_status");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(move || async move {
                load_eval_run_status_response(&pool, request.tenant_id, request.run_id)
                    .await
                    .map(Json::from)
                    .map_err(eval_error_to_handler_error)
            })
            .name("eval_run_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn datasets_register(
        &self,
        ctx: Context<'_>,
        request: Json<EvalDatasetRegisterRequest>,
    ) -> Result<Json<EvalDatasetRegisterResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "datasets_register");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                register_dataset_for_tenant(&pool, request)
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
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                list_datasets_for_tenant(&pool, request)
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
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;

        let config = self.config.as_ref().clone();
        let pool = self.pool.clone();

        run_replay_request_isolated(config, pool, request)
            .await
            .map(Json::from)
            .map_err(eval_error_to_handler_error)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn scores(
        &self,
        ctx: Context<'_>,
        request: Json<EvalScoresRequest>,
    ) -> Result<Json<EvalScoresResponse>, HandlerError> {
        annotate_restate_handler_span("Eval", "scores");
        let request = request.into_inner();
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                score_summaries_for_tenant(
                    &pool,
                    ScoreRunRef {
                        tenant_id: request.tenant_id,
                        run_id: request.run_id,
                    },
                )
                .await
                .map(|summary| eval_scores_response_from_summary(request.tenant_id, summary))
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
        authorize_tenant_operator_or_admin(&ctx, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                compare_score_runs_for_tenant(
                    &pool,
                    ScoreCompareRef {
                        tenant_id: request.tenant_id,
                        base_run: request.base_run,
                        new_run: request.new_run,
                    },
                )
                .await
                .map(|compare| eval_compare_response_from_scores(request.tenant_id, compare))
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
    /// A dataset item attempted to target a different tenant.
    #[error(
        "dataset item at line {line} targets tenant {item_tenant_id}, not authorized tenant {request_tenant_id}"
    )]
    DatasetTenantMismatch {
        /// One-indexed JSONL line number.
        line: usize,
        /// Authorized request tenant.
        request_tenant_id: TenantId,
        /// Tenant supplied by the dataset item.
        item_tenant_id: TenantId,
    },
    /// A dataset had no items in the authorized tenant.
    #[error("dataset {dataset_id} has no items in tenant {tenant_id}")]
    EmptyTenantDataset {
        /// Dataset identifier.
        dataset_id: Uuid,
        /// Authorized request tenant.
        tenant_id: TenantId,
    },
    /// A stored run belongs to a different tenant than the request.
    #[error("eval run {run_id} was not found in tenant {request_tenant_id}")]
    RunTenantMismatch {
        /// Hosted eval run identifier.
        run_id: Uuid,
        /// Authorized request tenant.
        request_tenant_id: TenantId,
    },
    /// A hosted eval worker dispatch did not carry the token created by `Eval/run`.
    #[error("eval run {run_id} was not dispatched by Eval/run")]
    InvalidInternalDispatch {
        /// Hosted eval run identifier.
        run_id: Uuid,
    },
}

impl From<moa_eval_core::EvalError> for EvalServiceError {
    fn from(error: moa_eval_core::EvalError) -> Self {
        Self::Eval(Box::new(error))
    }
}

/// Verifies that a workflow status response is scoped to the requested tenant.
pub fn verify_run_status_tenant(
    request_tenant_id: TenantId,
    response: &EvalRunStatusResponse,
) -> Result<(), EvalServiceError> {
    if response.tenant_id != request_tenant_id {
        return Err(EvalServiceError::RunTenantMismatch {
            run_id: response.run_id,
            request_tenant_id,
        });
    }
    Ok(())
}

/// Converts a terminal run response into a status-poll response.
#[must_use]
pub fn status_response_from_run_response(response: &EvalRunResponse) -> EvalRunStatusResponse {
    EvalRunStatusResponse {
        tenant_id: response.tenant_id,
        run_id: response.run_id,
        status: response.status,
        suite_name: Some(response.suite_name.clone()),
        exit_code: Some(response.exit_code),
        summary: Some(response.summary.clone()),
        results: response.results.clone(),
        error: response.error.clone(),
    }
}

/// Builds a non-terminal accepted run response after starting hosted execution.
#[must_use]
pub fn accepted_eval_run_response(
    tenant_id: TenantId,
    run_id: Uuid,
    suite_name: String,
) -> EvalRunResponse {
    EvalRunResponse {
        tenant_id,
        run_id,
        status: EvalRunStatus::Running,
        suite_name,
        exit_code: 2,
        summary: serde_json::json!({}),
        results: Vec::new(),
        error: None,
    }
}

fn generate_internal_eval_dispatch_token() -> String {
    format!("{}.{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn internal_eval_dispatch_token_hash(dispatch_token: &str) -> String {
    hex::encode(Sha256::digest(dispatch_token.as_bytes()))
}

fn eval_run_request_document(
    request: &EvalRunRequest,
    dispatch_token: &str,
) -> Result<Value, EvalServiceError> {
    let mut document = serde_json::to_value(request)?;
    let Some(object) = document.as_object_mut() else {
        return Err(EvalServiceError::Runtime {
            message: "eval run request did not serialize to an object".to_string(),
        });
    };
    object.insert(
        INTERNAL_EVAL_DISPATCH_TOKEN_HASH_FIELD.to_string(),
        Value::String(internal_eval_dispatch_token_hash(dispatch_token)),
    );
    Ok(document)
}

fn verify_internal_eval_dispatch_token(
    run_id: Uuid,
    dispatch_token: &str,
    stored_hash: Option<&str>,
) -> Result<(), EvalServiceError> {
    let Some(stored_hash) = stored_hash.filter(|value| !value.trim().is_empty()) else {
        return Err(EvalServiceError::InvalidInternalDispatch { run_id });
    };
    let candidate_hash = internal_eval_dispatch_token_hash(dispatch_token);
    if bool::from(candidate_hash.as_bytes().ct_eq(stored_hash.as_bytes())) {
        Ok(())
    } else {
        Err(EvalServiceError::InvalidInternalDispatch { run_id })
    }
}

/// Builds a failed terminal run response for errors before case-level results exist.
#[must_use]
pub fn failed_eval_run_response(
    tenant_id: TenantId,
    run_id: Uuid,
    error: impl Into<String>,
) -> EvalRunResponse {
    EvalRunResponse {
        tenant_id,
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

async fn persist_accepted_eval_run(
    pool: &PgPool,
    request: &EvalRunRequest,
    response: &EvalRunResponse,
    dispatch_token: &str,
) -> Result<(), EvalServiceError> {
    let request_document = eval_run_request_document(request, dispatch_token)?;
    let scope_context = RlsContext::tenant(request.tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
        .await
        .map_err(scoped_conn_error)?;
    sqlx::query(
        r#"
        INSERT INTO analytics.eval_run_status (
            run_id,
            tenant_id,
            status,
            request,
            response,
            error
        )
        VALUES ($1, $2, $3, $4, NULL, NULL)
        ON CONFLICT (run_id) DO UPDATE
        SET tenant_id = EXCLUDED.tenant_id,
            status = EXCLUDED.status,
            request = EXCLUDED.request,
            response = NULL,
            error = NULL,
            updated_at = now()
        "#,
    )
    .bind(response.run_id)
    .bind(response.tenant_id.0)
    .bind(eval_run_status_as_str(response.status))
    .bind(sqlx::types::Json(request_document))
    .execute(conn.as_mut())
    .await?;
    conn.commit().await.map_err(scoped_conn_error)?;
    Ok(())
}

async fn load_eval_run_execution_state(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<StoredEvalRunExecutionState, EvalServiceError> {
    let scope_context = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
        .await
        .map_err(scoped_conn_error)?;
    let row = sqlx::query(
        r#"
        SELECT status, response, request
        FROM analytics.eval_run_status
        WHERE run_id = $1 AND tenant_id = $2
        "#,
    )
    .bind(run_id)
    .bind(tenant_id.0)
    .fetch_optional(conn.as_mut())
    .await?
    .ok_or(EvalServiceError::RunTenantMismatch {
        run_id,
        request_tenant_id: tenant_id,
    })?;

    let status: String = row.try_get("status")?;
    let response: Option<sqlx::types::Json<EvalRunResponse>> = row.try_get("response")?;
    let request_document: sqlx::types::Json<Value> = row.try_get("request")?;
    let state = StoredEvalRunExecutionState {
        status: eval_run_status_from_str(&status)?,
        response: response.map(|json| json.0),
        dispatch_token_hash: request_document
            .0
            .get(INTERNAL_EVAL_DISPATCH_TOKEN_HASH_FIELD)
            .and_then(Value::as_str)
            .map(ToString::to_string),
    };
    conn.commit().await.map_err(scoped_conn_error)?;
    Ok(state)
}

async fn mark_eval_run_running(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<(), EvalServiceError> {
    let scope_context = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
        .await
        .map_err(scoped_conn_error)?;
    let result = sqlx::query(
        r#"
        UPDATE analytics.eval_run_status
        SET status = 'running',
            error = NULL,
            updated_at = now()
        WHERE run_id = $1
          AND tenant_id = $2
          AND status IN ('pending', 'running')
        "#,
    )
    .bind(run_id)
    .bind(tenant_id.0)
    .execute(conn.as_mut())
    .await?;
    if result.rows_affected() == 0 {
        return Err(EvalServiceError::RunTenantMismatch {
            run_id,
            request_tenant_id: tenant_id,
        });
    }
    conn.commit().await.map_err(scoped_conn_error)?;
    Ok(())
}

async fn persist_terminal_eval_run_response(
    pool: &PgPool,
    response: &EvalRunResponse,
) -> Result<(), EvalServiceError> {
    let scope_context = RlsContext::tenant(response.tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
        .await
        .map_err(scoped_conn_error)?;
    let result = sqlx::query(
        r#"
        UPDATE analytics.eval_run_status
        SET status = $3,
            response = $4,
            error = $5,
            updated_at = now()
        WHERE run_id = $1 AND tenant_id = $2
        "#,
    )
    .bind(response.run_id)
    .bind(response.tenant_id.0)
    .bind(eval_run_status_as_str(response.status))
    .bind(sqlx::types::Json(response.clone()))
    .bind(response.error.as_deref())
    .execute(conn.as_mut())
    .await?;
    if result.rows_affected() == 0 {
        return Err(EvalServiceError::RunTenantMismatch {
            run_id: response.run_id,
            request_tenant_id: response.tenant_id,
        });
    }
    conn.commit().await.map_err(scoped_conn_error)?;
    Ok(())
}

async fn load_eval_run_status_response(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<EvalRunStatusResponse, EvalServiceError> {
    let scope_context = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
        .await
        .map_err(scoped_conn_error)?;
    let row = sqlx::query(
        r#"
        SELECT tenant_id, run_id, status, response, error
        FROM analytics.eval_run_status
        WHERE run_id = $1 AND tenant_id = $2
        "#,
    )
    .bind(run_id)
    .bind(tenant_id.0)
    .fetch_optional(conn.as_mut())
    .await?
    .ok_or(EvalServiceError::RunTenantMismatch {
        run_id,
        request_tenant_id: tenant_id,
    })?;

    let stored_tenant_id: Uuid = row.try_get("tenant_id")?;
    let stored_run_id: Uuid = row.try_get("run_id")?;
    let status: String = row.try_get("status")?;
    let response: Option<sqlx::types::Json<EvalRunResponse>> = row.try_get("response")?;
    if let Some(response) = response {
        let status = status_response_from_run_response(&response.0);
        conn.commit().await.map_err(scoped_conn_error)?;
        return Ok(status);
    }
    let response = EvalRunStatusResponse {
        tenant_id: TenantId::from(stored_tenant_id),
        run_id: stored_run_id,
        status: eval_run_status_from_str(&status)?,
        suite_name: None,
        exit_code: None,
        summary: None,
        results: Vec::new(),
        error: row.try_get("error")?,
    };
    conn.commit().await.map_err(scoped_conn_error)?;
    Ok(response)
}

fn scoped_conn_error(error: moa_core::error::MoaError) -> EvalServiceError {
    EvalServiceError::Runtime {
        message: error.to_string(),
    }
}

fn normalize_eval_run_response(
    tenant_id: TenantId,
    run_id: Uuid,
    response: EvalRunResponse,
) -> EvalRunResponse {
    if response.run_id == run_id {
        response
    } else {
        failed_eval_run_response(
            tenant_id,
            run_id,
            format!("eval worker produced mismatched run id {}", response.run_id),
        )
    }
}

fn eval_run_status_as_str(status: EvalRunStatus) -> &'static str {
    match status {
        EvalRunStatus::Pending => "pending",
        EvalRunStatus::Running => "running",
        EvalRunStatus::Completed => "completed",
        EvalRunStatus::Failed => "failed",
    }
}

fn eval_run_status_from_str(status: &str) -> Result<EvalRunStatus, EvalServiceError> {
    match status {
        "pending" => Ok(EvalRunStatus::Pending),
        "running" => Ok(EvalRunStatus::Running),
        "completed" => Ok(EvalRunStatus::Completed),
        "failed" => Ok(EvalRunStatus::Failed),
        other => Err(EvalServiceError::InvalidDocument {
            document_source: "analytics.eval_run_status".to_string(),
            message: format!("stored eval run status is invalid: {other}"),
        }),
    }
}

fn eval_run_status_is_terminal(status: EvalRunStatus) -> bool {
    matches!(status, EvalRunStatus::Completed | EvalRunStatus::Failed)
}

/// Runs an already-authorized eval request inside the hosted eval worker.
pub async fn execute_eval_run_request(
    config: MoaConfig,
    run_id: Uuid,
    request: EvalRunRequest,
) -> EvalRunResponse {
    let tenant_id = request.tenant_id;
    match Box::pin(execute_eval_run_request_inner(config, run_id, request)).await {
        Ok(response) => response,
        Err(error) => failed_eval_run_response(tenant_id, run_id, error.to_string()),
    }
}

/// Runs an already-authorized eval request on an isolated current-thread runtime.
pub async fn execute_eval_run_request_isolated(
    config: MoaConfig,
    run_id: Uuid,
    request: EvalRunRequest,
) -> EvalRunResponse {
    let tenant_id = request.tenant_id;
    let join = tokio::task::spawn_blocking(move || {
        block_on_current_thread(Box::pin(execute_eval_run_request(config, run_id, request)))
    })
    .await;
    match join {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => failed_eval_run_response(tenant_id, run_id, error),
        Err(error) => failed_eval_run_response(tenant_id, run_id, error.to_string()),
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
    documents: Vec<moa_core::wire::eval::EvalSuiteListDocument>,
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
        tenant_id: request.tenant_id,
        suites: suite_summaries_from_documents(request.documents)?,
    })
}

async fn execute_eval_run_request_inner(
    config: MoaConfig,
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
        config,
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
        tenant_id: request.tenant_id,
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

fn attach_summary_field(summary: &mut Value, field: &str, value: Value) {
    if let Some(object) = summary.as_object_mut() {
        object.insert(field.to_string(), value);
    }
}

async fn replay_dataset_for_tenant(
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
    let items = load_dataset_items_for_tenant(
        &pool,
        &request.tenant_id,
        request.dataset_id,
        replay_config.limit,
    )
    .await?;
    if items.is_empty() {
        return Err(EvalServiceError::EmptyTenantDataset {
            dataset_id: request.dataset_id,
            tenant_id: request.tenant_id,
        });
    }
    ensure_eval_replay_score_run_parent(&pool, request.tenant_id, run_id).await?;

    let (sink, writer) = MpscSink::spawn(
        MpscSinkConfig::from(&config.observability.lineage),
        moa_lineage_sink::LineageStore::from_config(config.clickhouse.as_ref(), pool.clone()),
    )
    .await?;
    let report = Box::pin(replay_items_live(
        config,
        Arc::new(sink),
        replay_config,
        items,
    ))
    .await?;
    writer.shutdown().await?;
    Ok(EvalReplayResponse {
        tenant_id: request.tenant_id,
        run_id: report.run_id,
        dataset_id: report.dataset_id,
        items: u64::try_from(report.items)
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
        scores: u64::try_from(report.scores)
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "scores" })?,
    })
}

async fn ensure_eval_replay_score_run_parent(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<(), EvalServiceError> {
    let scope = ActionRuleScope::Tenant { tenant_id };
    let scope_context = RlsContext::tenant(tenant_id);
    let mut conn = ScopedConn::begin(pool, &scope_context)
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

async fn run_replay_request_isolated(
    config: MoaConfig,
    pool: PgPool,
    request: EvalReplayRequest,
) -> Result<EvalReplayResponse, EvalServiceError> {
    tokio::task::spawn_blocking(move || {
        block_on_current_thread(Box::pin(replay_dataset_for_tenant(config, pool, request)))
    })
    .await
    .map_err(|error| EvalServiceError::Runtime {
        message: error.to_string(),
    })?
    .map_err(|message| EvalServiceError::Runtime { message })?
}

#[derive(Clone, Debug)]
struct ScopedReplayReport {
    run_id: Uuid,
    dataset_id: Uuid,
    items: usize,
    scores: usize,
}

async fn replay_items_live(
    config: MoaConfig,
    sink: Arc<MpscSink>,
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
            sink.record_durable_event(LineageEvent::Eval(record))
                .await?;
        }
        report.items += 1;
    }

    Ok(report)
}

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
        storage_partition_id: StoragePartitionId::for_tenant(item.tenant_id),
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

fn replay_evaluator_name(cfg: &ReplayConfig) -> String {
    match (&cfg.model_override, &cfg.embedder_override) {
        (Some(model), Some(embedder)) => format!("replay-f1:{model}:{embedder}"),
        (Some(model), None) => format!("replay-f1:{model}"),
        (None, Some(embedder)) => format!("replay-f1:{embedder}"),
        (None, None) => "f1-overlap".to_string(),
    }
}

fn eval_error_to_handler_error(error: EvalServiceError) -> HandlerError {
    match error {
        EvalServiceError::InvalidDocument { .. }
        | EvalServiceError::DatasetTenantMismatch { .. }
        | EvalServiceError::IntegerTooLarge { .. } => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        EvalServiceError::InvalidInternalDispatch { .. } => {
            TerminalError::new_with_code(403, error.to_string()).into()
        }
        EvalServiceError::EmptyTenantDataset { .. }
        | EvalServiceError::RunTenantMismatch { .. } => {
            TerminalError::new_with_code(404, error.to_string()).into()
        }
        EvalServiceError::Json(_)
        | EvalServiceError::Eval(_)
        | EvalServiceError::Sql(_)
        | EvalServiceError::Lineage(_)
        | EvalServiceError::Runtime { .. } => TerminalError::new(error.to_string()).into(),
    }
}

fn eval_scores_response_from_summary(
    tenant_id: TenantId,
    summary: ScoreSummary,
) -> EvalScoresResponse {
    EvalScoresResponse {
        tenant_id,
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

fn eval_compare_response_from_scores(
    tenant_id: TenantId,
    compare: ScoreCompare,
) -> EvalCompareResponse {
    EvalCompareResponse {
        tenant_id,
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
        let tenant_id = TenantId::from(uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"));
        let item = ScopedDatasetItem {
            item_id,
            tenant_id,
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
            assert_eq!(
                record.storage_partition_id,
                StoragePartitionId::for_tenant(tenant_id)
            );
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
            tenant_id: TenantId::from(uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")),
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

    #[test]
    fn eval_run_status_strings_match_persisted_values() {
        // Pins: Postgres status strings decode to the wire lifecycle enum used by run_status.
        assert_eq!(eval_run_status_as_str(EvalRunStatus::Pending), "pending");
        assert_eq!(eval_run_status_as_str(EvalRunStatus::Running), "running");
        assert_eq!(
            eval_run_status_as_str(EvalRunStatus::Completed),
            "completed"
        );
        assert_eq!(eval_run_status_as_str(EvalRunStatus::Failed), "failed");
        assert_eq!(
            eval_run_status_from_str("pending").expect("pending status should decode"),
            EvalRunStatus::Pending
        );
        assert_eq!(
            eval_run_status_from_str("running").expect("running status should decode"),
            EvalRunStatus::Running
        );
        assert_eq!(
            eval_run_status_from_str("completed").expect("completed status should decode"),
            EvalRunStatus::Completed
        );
        assert_eq!(
            eval_run_status_from_str("failed").expect("failed status should decode"),
            EvalRunStatus::Failed
        );
        assert!(eval_run_status_is_terminal(EvalRunStatus::Completed));
        assert!(eval_run_status_is_terminal(EvalRunStatus::Failed));
        assert!(!eval_run_status_is_terminal(EvalRunStatus::Running));

        let error =
            eval_run_status_from_str("stale").expect_err("unknown persisted status should reject");
        match error {
            EvalServiceError::InvalidDocument {
                document_source,
                message,
            } => {
                assert_eq!(document_source, "analytics.eval_run_status");
                assert_eq!(message, "stored eval run status is invalid: stale");
            }
            other => panic!("expected invalid document error, got {other:?}"),
        }
    }

    #[test]
    fn eval_run_worker_mismatched_run_id_becomes_failed_response() {
        // Pins: terminal persistence never stores a worker response under the wrong accepted run id.
        let tenant_id = TenantId::from(uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"));
        let accepted_run_id = uuid("11111111-1111-1111-1111-111111111111");
        let worker_run_id = uuid("22222222-2222-2222-2222-222222222222");
        let worker_response = EvalRunResponse {
            tenant_id,
            run_id: worker_run_id,
            status: EvalRunStatus::Completed,
            suite_name: "suite".to_string(),
            exit_code: 0,
            summary: serde_json::json!({"passed": 1}),
            results: Vec::new(),
            error: None,
        };

        let normalized = normalize_eval_run_response(tenant_id, accepted_run_id, worker_response);

        assert_eq!(normalized.tenant_id, tenant_id);
        assert_eq!(normalized.run_id, accepted_run_id);
        assert_eq!(normalized.status, EvalRunStatus::Failed);
        assert_eq!(normalized.exit_code, 2);
        assert_eq!(
            normalized.error.as_deref(),
            Some("eval worker produced mismatched run id 22222222-2222-2222-2222-222222222222")
        );
    }

    #[test]
    fn internal_dispatch_guard_requires_matching_token_hash() {
        // Pins: Eval/execute_run cannot be used without the dispatch token generated by Eval/run.
        let run_id = uuid("11111111-1111-1111-1111-111111111111");
        let token = "server-issued-token";
        let stored_hash = internal_eval_dispatch_token_hash(token);

        verify_internal_eval_dispatch_token(run_id, token, Some(&stored_hash))
            .expect("matching dispatch token should pass");

        let missing = verify_internal_eval_dispatch_token(run_id, token, None)
            .expect_err("missing dispatch token hash should reject");
        assert!(matches!(
            missing,
            EvalServiceError::InvalidInternalDispatch { run_id: actual } if actual == run_id
        ));

        let wrong = verify_internal_eval_dispatch_token(run_id, "wrong-token", Some(&stored_hash))
            .expect_err("wrong dispatch token should reject");
        assert!(matches!(
            wrong,
            EvalServiceError::InvalidInternalDispatch { run_id: actual } if actual == run_id
        ));
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
