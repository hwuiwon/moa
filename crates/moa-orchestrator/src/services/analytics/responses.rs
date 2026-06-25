//! Response builders for analytics service wire DTOs.

use moa_core::wire::analytics::{
    CacheDailyMetricRow, CacheStatsResponse, SessionStatsResponse, TenantStatsResponse,
    ToolStatsRequest, ToolStatsResponse, ToolStatsRow,
};
use moa_core::{
    CacheDailyMetric, SessionAnalyticsSummary, TenantAnalyticsSummary, TenantId, ToolCallSummary,
};

/// Scope used by the tool-stats analytics API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatsScope {
    /// Tenant-restricted analytics visible to tenant operators.
    Tenant {
        /// Tenant whose tool calls are summarized.
        tenant_id: TenantId,
    },
    /// Deployment-wide aggregate analytics visible to authorized service operators.
    Deployment,
}

impl ToolStatsScope {
    /// Returns the tenant filter, when the request is tenant-scoped.
    #[must_use]
    pub fn tenant_id(&self) -> Option<&TenantId> {
        match self {
            Self::Tenant { tenant_id } => Some(tenant_id),
            Self::Deployment => None,
        }
    }
}

/// Returns the requested tool analytics scope.
#[must_use]
pub fn tool_stats_scope(request: &ToolStatsRequest) -> ToolStatsScope {
    match request.tenant_id {
        Some(tenant_id) => ToolStatsScope::Tenant { tenant_id },
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
        tenant_id: summary.tenant_id,
        contact_id: summary.contact_id,
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

/// Converts a core tenant analytics summary into the public wire response.
#[must_use]
pub fn tenant_stats_response_from_summary(summary: TenantAnalyticsSummary) -> TenantStatsResponse {
    TenantStatsResponse {
        tenant_id: summary.tenant_id,
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
    tenant_id: Option<TenantId>,
    rows: Vec<ToolCallSummary>,
) -> ToolStatsResponse {
    ToolStatsResponse {
        tenant_id,
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

/// Converts tenant and daily cache analytics into the public wire response.
#[must_use]
pub fn cache_stats_response_from_parts(
    summary: TenantAnalyticsSummary,
    daily: Vec<CacheDailyMetric>,
) -> CacheStatsResponse {
    CacheStatsResponse {
        tenant_id: summary.tenant_id,
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
                tenant_id: row.tenant_id,
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
