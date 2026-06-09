//! Restate service for protected session, workspace, tool, and cache analytics.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::IdentityType;
use moa_core::wire::{
    CacheDailyMetricRow, CacheStatsRequest, CacheStatsResponse, SessionStatsRequest,
    SessionStatsResponse, ToolStatsRequest, ToolStatsResponse, ToolStatsRow, WorkspaceStatsRequest,
    WorkspaceStatsResponse,
};
use moa_core::{
    CacheDailyMetric, MoaError, SessionAnalyticsSummary, ToolCallSummary,
    WorkspaceAnalyticsSummary, WorkspaceId,
};
use restate_sdk::prelude::*;

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
