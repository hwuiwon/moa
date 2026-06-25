//! Restate service for protected session, tenant, tool, and cache analytics.

mod authz;
mod experiment_stats;
mod redaction;
mod responses;
mod session_search;

pub use experiment_stats::experiment_stats_response_from_parts;
pub use redaction::{redacted_event_snippet, redacted_payload_preview};
pub use responses::{
    ToolStatsScope, cache_stats_response_from_parts, session_stats_response_from_summary,
    tenant_stats_response_from_summary, tool_stats_response_from_rows, tool_stats_scope,
};
pub use session_search::session_search_response_from_events;

use moa_core::traits::SessionStore as _;
use moa_core::wire::analytics::{
    CacheStatsRequest, CacheStatsResponse, ExperimentAnalyticsRequest, ExperimentAnalyticsResponse,
    LearningCandidateListRequest, LearningCandidateListResponse, SessionSearchRequest,
    SessionSearchResponse, SessionStatsRequest, SessionStatsResponse, TenantStatsRequest,
    TenantStatsResponse, ToolStatsRequest, ToolStatsResponse,
};
use moa_core::{EventFilter, MoaError};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;

use self::authz::{
    authorize_deployment_operator, authorize_session_participant, authorize_tenant_admin,
    authorize_tenant_member,
};
use self::experiment_stats::experiment_stats_inner;

/// Restate service surface for protected analytics reports.
#[restate_sdk::service]
#[name = "Analytics"]
pub trait Analytics {
    /// Loads analytics for one session after a session participant check.
    async fn session_stats(
        request: Json<SessionStatsRequest>,
    ) -> Result<Json<SessionStatsResponse>, HandlerError>;

    /// Loads tenant analytics after a tenant admin check.
    async fn tenant_stats(
        request: Json<TenantStatsRequest>,
    ) -> Result<Json<TenantStatsResponse>, HandlerError>;

    /// Loads per-tool analytics after a tenant operator check.
    async fn tool_stats(
        request: Json<ToolStatsRequest>,
    ) -> Result<Json<ToolStatsResponse>, HandlerError>;

    /// Loads tenant cache analytics after a tenant admin check.
    async fn cache_stats(
        request: Json<CacheStatsRequest>,
    ) -> Result<Json<CacheStatsResponse>, HandlerError>;

    /// Loads tenant-scoped live experiment analytics after a tenant operator check.
    async fn experiment_stats(
        request: Json<ExperimentAnalyticsRequest>,
    ) -> Result<Json<ExperimentAnalyticsResponse>, HandlerError>;

    /// Lists curated learning-candidate summaries after a tenant operator or tenant admin check.
    async fn learning_candidates(
        request: Json<LearningCandidateListRequest>,
    ) -> Result<Json<LearningCandidateListResponse>, HandlerError>;

    /// Searches tenant session events and returns redacted snippets after a tenant operator check.
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
        let store = OrchestratorCtx::current_session_store();

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
    async fn tenant_stats(
        &self,
        ctx: Context<'_>,
        request: Json<TenantStatsRequest>,
    ) -> Result<Json<TenantStatsResponse>, HandlerError> {
        annotate_restate_handler_span("Analytics", "tenant_stats");
        let request = request.into_inner();
        authorize_tenant_admin(&ctx, request.tenant_id).await?;
        let store = OrchestratorCtx::current_session_store();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_analytics_materialized_views()
                    .await
                    .map_err(to_handler_error)?;
                let summary = store
                    .get_tenant_stats_control_plane(&request.tenant_id, request.days)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(tenant_stats_response_from_summary(summary)))
            })
            .name("analytics_tenant_stats")
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
            ToolStatsScope::Tenant { tenant_id } => {
                authorize_tenant_member(&ctx, *tenant_id).await?;
            }
            ToolStatsScope::Deployment => {
                authorize_deployment_operator(&ctx).await?;
            }
        }
        let store = OrchestratorCtx::current_session_store();

        Ok(ctx
            .run(|| async move {
                let tenant_id = scope.tenant_id().copied();
                let rows = store
                    .list_tool_call_summaries(tenant_id.as_ref())
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(tool_stats_response_from_rows(tenant_id, rows)))
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
        authorize_tenant_admin(&ctx, request.tenant_id).await?;
        let store = OrchestratorCtx::current_session_store();

        Ok(ctx
            .run(|| async move {
                store
                    .refresh_analytics_materialized_views()
                    .await
                    .map_err(to_handler_error)?;
                let summary = store
                    .get_tenant_stats_control_plane(&request.tenant_id, request.days)
                    .await
                    .map_err(to_handler_error)?;
                let daily = store
                    .list_cache_daily_metrics_control_plane(&request.tenant_id, request.days)
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
        authorize_tenant_member(&ctx, request.tenant_id).await?;
        let pool = OrchestratorCtx::current_graph_pool();

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
        let tenant_id = request.tenant_id;
        authorize_tenant_member(&ctx, tenant_id).await?;
        let store = OrchestratorCtx::current_session_store();

        Ok(ctx
            .run(|| async move {
                let candidates = store
                    .list_learning_candidate_summaries(tenant_id, request.status, request.limit)
                    .await
                    .map_err(to_handler_error)?;
                Ok(Json(LearningCandidateListResponse {
                    tenant_id,
                    candidates,
                }))
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
        authorize_tenant_member(&ctx, request.tenant_id).await?;
        let store = OrchestratorCtx::current_session_store();

        Ok(ctx
            .run(|| async move {
                let events = store
                    .search_events(
                        &request.query,
                        EventFilter {
                            session_id: None,
                            tenant_id: Some(request.tenant_id),
                            contact_id: None,
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

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}
