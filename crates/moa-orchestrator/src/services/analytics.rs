//! Restate service for protected session, workspace, tool, and cache analytics.

use std::sync::OnceLock;

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{IdentityType, SessionStore as _};
use moa_core::wire::{
    CacheDailyMetricRow, CacheStatsRequest, CacheStatsResponse, ExperimentAnalyticsRequest,
    ExperimentAnalyticsResponse, ExperimentRunTrendPoint, ExperimentScoreRunRef,
    ExperimentStatusCount, ExperimentTrialTrendPoint, LearningCandidateListRequest,
    LearningCandidateListResponse, LearningCandidateSummary, SessionSearchRequest,
    SessionSearchResponse, SessionSearchResult, SessionStatsRequest, SessionStatsResponse,
    ToolStatsRequest, ToolStatsResponse, ToolStatsRow, WorkspaceStatsRequest,
    WorkspaceStatsResponse,
};
use moa_core::{
    CacheDailyMetric, Event, EventFilter, EventRecord, LearningCandidateStatus,
    LearningCandidateType, LearningRiskClass, MemoryScope, MoaError, ScopeContext, ScopedConn,
    SessionAnalyticsSummary, ToolCallSummary, UserId, WorkspaceAnalyticsSummary, WorkspaceId,
};
use regex::Regex;
use restate_sdk::prelude::*;
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder, Row, postgres::PgRow};

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Restate service surface for protected analytics reports.
#[restate_sdk::service]
#[name = "Analytics"]
pub trait Analytics {
    /// Loads analytics for one session after a session participant check.
    async fn session_stats(
        request: Json<SessionStatsRequest>,
    ) -> Result<Json<SessionStatsResponse>, HandlerError>;

    /// Loads workspace analytics after a workspace member check.
    async fn workspace_stats(
        request: Json<WorkspaceStatsRequest>,
    ) -> Result<Json<WorkspaceStatsResponse>, HandlerError>;

    /// Loads per-tool analytics after a workspace member check.
    async fn tool_stats(
        request: Json<ToolStatsRequest>,
    ) -> Result<Json<ToolStatsResponse>, HandlerError>;

    /// Loads workspace cache analytics after a workspace member check.
    async fn cache_stats(
        request: Json<CacheStatsRequest>,
    ) -> Result<Json<CacheStatsResponse>, HandlerError>;

    /// Loads workspace-scoped live experiment analytics after a workspace member check.
    async fn experiment_stats(
        request: Json<ExperimentAnalyticsRequest>,
    ) -> Result<Json<ExperimentAnalyticsResponse>, HandlerError>;

    /// Lists curated learning-candidate summaries after a workspace editor or tenant admin check.
    async fn learning_candidates(
        request: Json<LearningCandidateListRequest>,
    ) -> Result<Json<LearningCandidateListResponse>, HandlerError>;

    /// Searches workspace session events and returns redacted snippets after a workspace member check.
    async fn session_search(
        request: Json<SessionSearchRequest>,
    ) -> Result<Json<SessionSearchResponse>, HandlerError>;
}

/// Concrete analytics service implementation.
#[derive(Clone, Default)]
pub struct AnalyticsImpl;

impl Analytics for AnalyticsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn session_stats(
        &self,
        ctx: Context<'_>,
        request: Json<SessionStatsRequest>,
    ) -> Result<Json<SessionStatsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "session_stats");
        let request = request.into_inner();
        authorize_session_participant(&ctx, request.session_id).await?;
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                let summary = store
                    .get_session_summary(request.session_id)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(session_stats_response_from_summary(summary)))
            })
            .name("analytics_session_stats")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn workspace_stats(
        &self,
        ctx: Context<'_>,
        request: Json<WorkspaceStatsRequest>,
    ) -> Result<Json<WorkspaceStatsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "workspace_stats");
        let request = request.into_inner();
        authorize_workspace_member(&ctx, &request.workspace_id).await?;
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_analytics_materialized_views()
                    .await
                    .map_err(to_handler_error)?;
                let summary = store
                    .get_workspace_stats(&request.workspace_id, request.days)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(workspace_stats_response_from_summary(summary)))
            })
            .name("analytics_workspace_stats")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn tool_stats(
        &self,
        ctx: Context<'_>,
        request: Json<ToolStatsRequest>,
    ) -> Result<Json<ToolStatsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "tool_stats");
        let request = request.into_inner();
        let scope = tool_stats_scope(&request);
        match &scope {
            ToolStatsScope::Workspace { workspace_id } => {
                authorize_workspace_member(&ctx, workspace_id).await?;
            }
            ToolStatsScope::Deployment => {
                authorize_deployment_operator(&ctx).await?;
            }
        }
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                let workspace_id = scope.workspace_id().cloned();
                let rows = store
                    .list_tool_call_summaries(workspace_id.as_ref())
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(tool_stats_response_from_rows(workspace_id, rows)))
            })
            .name("analytics_tool_stats")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cache_stats(
        &self,
        ctx: Context<'_>,
        request: Json<CacheStatsRequest>,
    ) -> Result<Json<CacheStatsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "cache_stats");
        let request = request.into_inner();
        authorize_workspace_member(&ctx, &request.workspace_id).await?;
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_analytics_materialized_views()
                    .await
                    .map_err(to_handler_error)?;
                let summary = store
                    .get_workspace_stats(&request.workspace_id, request.days)
                    .await
                    .map_err(to_handler_error)?;
                let daily = store
                    .list_cache_daily_metrics(&request.workspace_id, request.days)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(cache_stats_response_from_parts(summary, daily)))
            })
            .name("analytics_cache_stats")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn experiment_stats(
        &self,
        ctx: Context<'_>,
        request: Json<ExperimentAnalyticsRequest>,
    ) -> Result<Json<ExperimentAnalyticsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "experiment_stats");
        let request = request.into_inner();
        authorize_workspace_member(&ctx, &request.workspace_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { experiment_stats_inner(pool, request).await.map(Json::from) })
            .name("analytics_experiment_stats")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn learning_candidates(
        &self,
        ctx: Context<'_>,
        request: Json<LearningCandidateListRequest>,
    ) -> Result<Json<LearningCandidateListResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "learning_candidates");
        let request = request.into_inner();
        let tenant_id = match learning_candidate_scope(&request) {
            LearningCandidateReadScope::Workspace { workspace_id } => {
                authorize_workspace_editor(&ctx, &workspace_id).await?;
                require_identity(&ctx)?.tenant_id.to_string()
            }
            LearningCandidateReadScope::Tenant => authorize_tenant_admin(&ctx).await?,
        };
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                learning_candidates_inner(
                    store.pool().clone(),
                    store.schema_name().map(str::to_string),
                    tenant_id,
                    request,
                )
                .await
                .map(Json::from)
            })
            .name("analytics_learning_candidates")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn session_search(
        &self,
        ctx: Context<'_>,
        request: Json<SessionSearchRequest>,
    ) -> Result<Json<SessionSearchResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "session_search");
        let request = request.into_inner();
        authorize_workspace_member(&ctx, &request.workspace_id).await?;
        let store = OrchestratorCtx::current().session_store.clone();

        Ok(ctx
            .run(|| async move {
                let events = store
                    .search_events(
                        &request.query,
                        EventFilter {
                            session_id: None,
                            workspace_id: Some(request.workspace_id.clone()),
                            user_id: None,
                            event_types: request.event_types.clone(),
                            from_time: request.from_time,
                            to_time: request.to_time,
                            limit: Some(request.limit as usize),
                        },
                    )
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(session_search_response_from_events(request, events)))
            })
            .name("analytics_session_search")
            .await?)
    }
}

/// Scope used by the tool-stats analytics API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatsScope {
    /// Workspace-restricted analytics visible to workspace members.
    Workspace {
        /// Workspace whose tool calls are summarized.
        workspace_id: WorkspaceId,
    },
    /// Deployment-wide aggregate analytics visible to authorized service operators.
    Deployment,
}

impl ToolStatsScope {
    /// Returns the workspace filter, when the request is workspace-scoped.
    #[must_use]
    pub fn workspace_id(&self) -> Option<&WorkspaceId> {
        match self {
            Self::Workspace { workspace_id } => Some(workspace_id),
            Self::Deployment => None,
        }
    }
}

/// Scope used by the learning-candidate analytics API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningCandidateReadScope {
    /// Workspace-restricted candidates visible to workspace editors.
    Workspace {
        /// Workspace whose learning candidates are listed.
        workspace_id: WorkspaceId,
    },
    /// Tenant-wide candidates visible to tenant admins.
    Tenant,
}

/// Returns the requested tool analytics scope.
#[must_use]
pub fn tool_stats_scope(request: &ToolStatsRequest) -> ToolStatsScope {
    match &request.workspace_id {
        Some(workspace_id) => ToolStatsScope::Workspace {
            workspace_id: workspace_id.clone(),
        },
        None => ToolStatsScope::Deployment,
    }
}

/// Returns the requested learning-candidate analytics scope.
#[must_use]
pub fn learning_candidate_scope(
    request: &LearningCandidateListRequest,
) -> LearningCandidateReadScope {
    match &request.workspace_id {
        Some(workspace_id) => LearningCandidateReadScope::Workspace {
            workspace_id: workspace_id.clone(),
        },
        None => LearningCandidateReadScope::Tenant,
    }
}

/// Converts a core session analytics summary into the public wire response.
#[must_use]
pub fn session_stats_response_from_summary(
    summary: SessionAnalyticsSummary,
) -> SessionStatsResponse {
    SessionStatsResponse {
        session_id: summary.session_id,
        workspace_id: summary.workspace_id,
        user_id: summary.user_id,
        status: summary.status,
        turn_count: summary.turn_count,
        event_count: summary.event_count,
        total_input_tokens: summary.total_input_tokens,
        total_output_tokens: summary.total_output_tokens,
        total_cost_cents: summary.total_cost_cents,
        main_cost_cents: summary.main_cost_cents,
        auxiliary_cost_cents: summary.auxiliary_cost_cents,
        cache_hit_rate: summary.cache_hit_rate,
        duration_seconds: summary.duration_seconds,
        tool_call_count: summary.tool_call_count,
        error_count: summary.error_count,
    }
}

/// Converts a core workspace analytics summary into the public wire response.
#[must_use]
pub fn workspace_stats_response_from_summary(
    summary: WorkspaceAnalyticsSummary,
) -> WorkspaceStatsResponse {
    WorkspaceStatsResponse {
        workspace_id: summary.workspace_id,
        days: summary.days,
        session_count: summary.session_count,
        turn_count: summary.turn_count,
        total_input_tokens: summary.total_input_tokens,
        total_cache_read_tokens: summary.total_cache_read_tokens,
        total_output_tokens: summary.total_output_tokens,
        total_cost_cents: summary.total_cost_cents,
        cache_hit_rate: summary.cache_hit_rate,
    }
}

/// Converts core per-tool analytics rows into the public wire response.
#[must_use]
pub fn tool_stats_response_from_rows(
    workspace_id: Option<WorkspaceId>,
    rows: Vec<ToolCallSummary>,
) -> ToolStatsResponse {
    ToolStatsResponse {
        workspace_id,
        rows: rows
            .into_iter()
            .map(|row| ToolStatsRow {
                tool_name: row.tool_name,
                call_count: row.call_count,
                success_rate: row.success_rate,
                avg_duration_ms: row.avg_duration_ms,
                p50_ms: row.p50_ms,
                p95_ms: row.p95_ms,
            })
            .collect(),
    }
}

/// Converts workspace and daily cache analytics into the public wire response.
#[must_use]
pub fn cache_stats_response_from_parts(
    summary: WorkspaceAnalyticsSummary,
    daily: Vec<CacheDailyMetric>,
) -> CacheStatsResponse {
    CacheStatsResponse {
        workspace_id: summary.workspace_id,
        days: summary.days,
        cache_hit_rate: summary.cache_hit_rate,
        total_cache_read_tokens: summary.total_cache_read_tokens,
        total_input_tokens: summary.total_input_tokens,
        total_output_tokens: summary.total_output_tokens,
        total_cost_cents: summary.total_cost_cents,
        estimated_savings_cents: None,
        daily: daily
            .into_iter()
            .map(|row| CacheDailyMetricRow {
                workspace_id: row.workspace_id,
                day: row.day,
                session_count: row.session_count,
                turn_count: row.turn_count,
                total_input_tokens: row.total_input_tokens,
                total_cache_read_tokens: row.total_cache_read_tokens,
                total_output_tokens: row.total_output_tokens,
                total_cost_cents: row.total_cost_cents,
                avg_cache_hit_rate: row.avg_cache_hit_rate,
            })
            .collect(),
    }
}

async fn experiment_stats_inner(
    pool: sqlx::PgPool,
    request: ExperimentAnalyticsRequest,
) -> Result<ExperimentAnalyticsResponse, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id.clone(),
    };
    let mut conn = ScopedConn::begin(&pool, &ScopeContext::from(scope))
        .await
        .map_err(to_handler_error)?;
    let status_rows = sqlx::query(
        r#"
        SELECT status, COUNT(*)::BIGINT AS count
        FROM moa.experiment_run
        WHERE scope = 'workspace'
          AND workspace_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY status
        ORDER BY status
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_to_handler_error)?;
    let score_rows = sqlx::query(
        r#"
        SELECT run_uid, name, status, score_run_id, created_at
        FROM moa.experiment_run
        WHERE scope = 'workspace'
          AND workspace_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        ORDER BY created_at DESC, run_uid ASC
        LIMIT $4
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(request.from_time)
    .bind(request.to_time)
    .bind(i64::from(request.limit))
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_to_handler_error)?;
    let run_trend_rows = sqlx::query(
        r#"
        SELECT date_trunc('day', created_at) AS day,
               status,
               COUNT(*)::BIGINT AS count
        FROM moa.experiment_run
        WHERE scope = 'workspace'
          AND workspace_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY day, status
        ORDER BY day ASC, status
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_to_handler_error)?;
    let trial_trend_rows = sqlx::query(
        r#"
        SELECT date_trunc('day', created_at) AS day,
               status,
               variant_key,
               scenario_id,
               COUNT(*)::BIGINT AS count
        FROM moa.experiment_trial
        WHERE scope = 'workspace'
          AND workspace_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY day, status, variant_key, scenario_id
        ORDER BY day ASC,
                 status,
                 variant_key,
                 scenario_id ASC NULLS FIRST
        "#,
    )
    .bind(request.workspace_id.as_str())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_to_handler_error)?;
    conn.commit().await.map_err(to_handler_error)?;

    let statuses = status_rows
        .iter()
        .map(experiment_status_count_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let score_runs = score_rows
        .iter()
        .map(experiment_score_run_ref_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let run_trends = run_trend_rows
        .iter()
        .map(experiment_run_trend_point_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let trial_trends = trial_trend_rows
        .iter()
        .map(experiment_trial_trend_point_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(experiment_stats_response_from_parts(
        request.workspace_id,
        statuses,
        score_runs,
        run_trends,
        trial_trends,
    ))
}

async fn learning_candidates_inner(
    pool: sqlx::PgPool,
    schema_name: Option<String>,
    tenant_id: String,
    request: LearningCandidateListRequest,
) -> Result<LearningCandidateListResponse, HandlerError> {
    let learning_candidates = qualified_table(schema_name.as_deref(), "learning_candidates");
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT id, tenant_id, workspace_id, user_id, candidate_type, status, \
         target_id, target_label, task_fingerprint, payload, \
         confidence::DOUBLE PRECISION AS confidence, risk_class, created_at, updated_at \
         FROM {learning_candidates} WHERE tenant_id = "
    ));
    query.push_bind(tenant_id.as_str());
    if let Some(workspace_id) = &request.workspace_id {
        query.push(" AND workspace_id = ");
        query.push_bind(workspace_id.as_str());
    }
    if let Some(status) = request.status {
        query.push(" AND status = ");
        query.push_bind(status.as_str());
    }
    query.push(" ORDER BY updated_at DESC, id ASC LIMIT ");
    query.push_bind(i64::from(request.limit));

    let rows = query
        .build()
        .fetch_all(&pool)
        .await
        .map_err(sqlx_to_handler_error)?;
    let candidates = rows
        .iter()
        .map(learning_candidate_summary_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(LearningCandidateListResponse {
        tenant_id,
        workspace_id: request.workspace_id,
        candidates,
    })
}

/// Converts experiment status counts and score-run references into a response.
#[must_use]
pub fn experiment_stats_response_from_parts(
    workspace_id: WorkspaceId,
    statuses: Vec<ExperimentStatusCount>,
    score_runs: Vec<ExperimentScoreRunRef>,
    run_trends: Vec<ExperimentRunTrendPoint>,
    trial_trends: Vec<ExperimentTrialTrendPoint>,
) -> ExperimentAnalyticsResponse {
    let total_runs = statuses.iter().map(|row| row.count).sum();
    ExperimentAnalyticsResponse {
        workspace_id,
        total_runs,
        statuses,
        score_runs,
        run_trends,
        trial_trends,
    }
}

/// Converts event search records into redacted public search results.
#[must_use]
pub fn session_search_response_from_events(
    request: SessionSearchRequest,
    events: Vec<EventRecord>,
) -> SessionSearchResponse {
    SessionSearchResponse {
        workspace_id: request.workspace_id,
        query: request.query,
        results: events
            .iter()
            .map(|event| SessionSearchResult {
                session_id: event.session_id,
                event_id: event.id,
                sequence_num: event.sequence_num,
                event_type: event.event_type.clone(),
                timestamp: event.timestamp,
                snippet: redacted_event_snippet(&event.event),
            })
            .collect(),
    }
}

/// Builds a short redacted snippet for a session event.
#[must_use]
pub fn redacted_event_snippet(event: &Event) -> String {
    let text = match event {
        Event::SessionCreated {
            workspace_id,
            user_id,
            model,
        } => format!("session created in workspace {workspace_id} by {user_id} using {model}"),
        Event::SessionStatusChanged { from, to } => {
            format!("session status changed from {from:?} to {to:?}")
        }
        Event::SessionCompleted {
            summary,
            total_turns,
        } => format!("session completed after {total_turns} turns: {summary}"),
        Event::SegmentStarted { task_summary, .. }
        | Event::SegmentCompleted { task_summary, .. } => task_summary
            .clone()
            .unwrap_or_else(|| event.type_name().to_string()),
        Event::UserMessage { text, .. }
        | Event::QueuedMessage { text, .. }
        | Event::BrainResponse { text, .. }
        | Event::SubAgentMessageSent { text, .. } => text.clone(),
        Event::BrainThinking { summary, .. }
        | Event::SubAgentStatusChanged {
            summary: Some(summary),
            ..
        }
        | Event::SubAgentNotificationDelivered { summary, .. }
        | Event::MemoryWrite { summary, .. }
        | Event::Checkpoint { summary, .. } => summary.clone(),
        Event::ToolCall { tool_name, .. } => format!("tool call {tool_name} input redacted"),
        Event::ToolResult {
            success,
            duration_ms,
            ..
        } => format!("tool result success={success} duration_ms={duration_ms} output redacted"),
        Event::ToolError {
            tool_name, error, ..
        } => format!("tool error from {tool_name}: {error}"),
        Event::ActionReviewRequested { envelope, .. } => format!(
            "action review requested for {} risk={:?}: {}",
            envelope.tool_name, envelope.risk_level, envelope.input_summary
        ),
        Event::ActionReviewDecided { decision, .. } => {
            format!("action review decided: {decision:?}")
        }
        Event::SubAgentSpawned { path, task, .. } => {
            format!("sub-agent {path} spawned for task: {task}")
        }
        Event::SubAgentStatusChanged { to, .. } => {
            format!("sub-agent status changed to {to:?}")
        }
        Event::MemoryRead { path, scope } => format!("memory read {path} in {scope}"),
        Event::MemoryIngest {
            source_name,
            affected_pages,
            contradictions,
            ..
        } => format!(
            "memory ingest from {source_name}: {} pages affected, {} contradictions",
            affected_pages.len(),
            contradictions.len()
        ),
        Event::HandProvisioned {
            hand_id, provider, ..
        } => {
            format!("hand {hand_id} provisioned by {provider}")
        }
        Event::HandDestroyed { hand_id, reason } => {
            format!("hand {hand_id} destroyed: {reason}")
        }
        Event::HandError { hand_id, error } => format!("hand {hand_id} error: {error}"),
        Event::CacheReport { report } => format!("cache report: {report:?}"),
        Event::Error { message, .. } | Event::Warning { message } => message.clone(),
    };
    truncate_snippet(&redact_sensitive_text(&text), 240)
}

/// Builds a short redacted preview for a dynamic JSON payload.
#[must_use]
pub fn redacted_payload_preview(value: &Value) -> String {
    let redacted = redact_json_value(value);
    truncate_snippet(&redact_sensitive_text(&redacted.to_string()), 240)
}

fn experiment_status_count_from_row(row: &PgRow) -> Result<ExperimentStatusCount, HandlerError> {
    Ok(ExperimentStatusCount {
        status: row.try_get("status").map_err(sqlx_to_handler_error)?,
        count: u64_from_i64(
            row.try_get("count").map_err(sqlx_to_handler_error)?,
            "count",
        )?,
    })
}

fn experiment_score_run_ref_from_row(row: &PgRow) -> Result<ExperimentScoreRunRef, HandlerError> {
    Ok(ExperimentScoreRunRef {
        run_uid: row.try_get("run_uid").map_err(sqlx_to_handler_error)?,
        name: row.try_get("name").map_err(sqlx_to_handler_error)?,
        status: row.try_get("status").map_err(sqlx_to_handler_error)?,
        score_run_id: row.try_get("score_run_id").map_err(sqlx_to_handler_error)?,
        created_at: row.try_get("created_at").map_err(sqlx_to_handler_error)?,
    })
}

fn experiment_run_trend_point_from_row(
    row: &PgRow,
) -> Result<ExperimentRunTrendPoint, HandlerError> {
    Ok(ExperimentRunTrendPoint {
        day: row.try_get("day").map_err(sqlx_to_handler_error)?,
        status: row.try_get("status").map_err(sqlx_to_handler_error)?,
        count: u64_from_i64(
            row.try_get("count").map_err(sqlx_to_handler_error)?,
            "count",
        )?,
    })
}

fn experiment_trial_trend_point_from_row(
    row: &PgRow,
) -> Result<ExperimentTrialTrendPoint, HandlerError> {
    Ok(ExperimentTrialTrendPoint {
        day: row.try_get("day").map_err(sqlx_to_handler_error)?,
        status: row.try_get("status").map_err(sqlx_to_handler_error)?,
        variant_key: row.try_get("variant_key").map_err(sqlx_to_handler_error)?,
        scenario_id: row.try_get("scenario_id").map_err(sqlx_to_handler_error)?,
        count: u64_from_i64(
            row.try_get("count").map_err(sqlx_to_handler_error)?,
            "count",
        )?,
    })
}

fn learning_candidate_summary_from_row(
    row: &PgRow,
) -> Result<LearningCandidateSummary, HandlerError> {
    let candidate_type = parse_db_enum::<LearningCandidateType>(
        "learning candidate type",
        row.try_get("candidate_type")
            .map_err(sqlx_to_handler_error)?,
    )?;
    let status = parse_db_enum::<LearningCandidateStatus>(
        "learning candidate status",
        row.try_get("status").map_err(sqlx_to_handler_error)?,
    )?;
    let risk_class = parse_db_enum::<LearningRiskClass>(
        "learning risk class",
        row.try_get("risk_class").map_err(sqlx_to_handler_error)?,
    )?;
    let payload: Value = row.try_get("payload").map_err(sqlx_to_handler_error)?;
    Ok(LearningCandidateSummary {
        id: row.try_get("id").map_err(sqlx_to_handler_error)?,
        tenant_id: row.try_get("tenant_id").map_err(sqlx_to_handler_error)?,
        workspace_id: WorkspaceId::new(
            row.try_get::<String, _>("workspace_id")
                .map_err(sqlx_to_handler_error)?,
        ),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(sqlx_to_handler_error)?
            .map(UserId::new),
        candidate_type,
        status,
        target_id: row.try_get("target_id").map_err(sqlx_to_handler_error)?,
        target_label: row.try_get("target_label").map_err(sqlx_to_handler_error)?,
        task_fingerprint: row
            .try_get("task_fingerprint")
            .map_err(sqlx_to_handler_error)?,
        confidence: row.try_get("confidence").map_err(sqlx_to_handler_error)?,
        risk_class,
        payload_preview: redacted_payload_preview(&payload),
        created_at: row.try_get("created_at").map_err(sqlx_to_handler_error)?,
        updated_at: row.try_get("updated_at").map_err(sqlx_to_handler_error)?,
    })
}

fn parse_db_enum<T>(field: &'static str, value: String) -> Result<T, HandlerError>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| TerminalError::new(format!("invalid {field} `{value}`")).into())
}

fn u64_from_i64(value: i64, field: &'static str) -> Result<u64, HandlerError> {
    u64::try_from(value)
        .map_err(|_| TerminalError::new(format!("{field} was negative: {value}")).into())
}

fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = if is_sensitive_key(key) {
                        Value::String("[redacted]".to_string())
                    } else {
                        redact_json_value(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_json_value).collect()),
        Value::String(text) => Value::String(redact_sensitive_text(text)),
        _ => value.clone(),
    }
}

fn redact_sensitive_text(text: &str) -> String {
    let redacted = sensitive_text_patterns()
        .iter()
        .fold(text.to_string(), |redacted, pattern| {
            pattern.replace_all(&redacted, "[redacted]").into_owned()
        });
    redacted
        .split_whitespace()
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            if lower.starts_with("sk-")
                || lower.starts_with("ghp_")
                || lower.starts_with("bearer")
                || lower.contains("password=")
                || lower.contains("token=")
                || lower.contains("api_key=")
                || lower.contains("apikey=")
                || lower.contains("secret=")
            {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sensitive_text_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]+=*",
            r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
            r"\bAKIA[0-9A-Z]{16}\b",
            r"\bAIza[0-9A-Za-z_-]{20,}\b",
            r"\bsk-[A-Za-z0-9_-]{12,}\b",
            r"\bghp_[A-Za-z0-9_]{12,}\b",
            r"(?i)\b(password|token|api_key|apikey|secret)=([^&\s]+)",
        ]
        .into_iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
    })
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.contains("apikey")
        || key == "authorization"
        || key == "auth"
}

fn truncate_snippet(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = 0;
    for (index, _) in text.char_indices() {
        if index > limit {
            break;
        }
        end = index;
    }
    format!("{}...", &text[..end])
}

fn qualified_table(schema_name: Option<&str>, table_name: &str) -> String {
    match schema_name {
        Some(schema_name) => format!(
            "{}.{}",
            quote_identifier(schema_name),
            quote_identifier(table_name)
        ),
        None => quote_identifier(table_name),
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn sqlx_to_handler_error(error: sqlx::Error) -> HandlerError {
    to_handler_error(MoaError::StorageError(error.to_string()))
}

async fn authorize_session_participant(
    ctx: &impl RequestHeaders,
    session_id: moa_core::SessionId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)
}

async fn authorize_workspace_member(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Member,
    )
    .await
    .map_err(translate_authz_error)
}

async fn authorize_workspace_editor(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Editor,
    )
    .await
    .map_err(translate_authz_error)
}

async fn authorize_tenant_admin(ctx: &impl RequestHeaders) -> Result<String, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity.tenant_id.to_string())
}

async fn authorize_deployment_operator(ctx: &impl RequestHeaders) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    if identity.identity_type != IdentityType::Service {
        return Err(TerminalError::new_with_code(
            403,
            "deployment-wide tool stats require a service identity",
        )
        .into());
    }
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}
