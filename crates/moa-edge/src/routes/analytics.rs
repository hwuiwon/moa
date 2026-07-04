//! Public analytics, experiments, admin, lineage, and privacy routes.
#![allow(clippy::result_large_err)]

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::{IdentityType, SessionStore};
use moa_core::wire::analytics::{
    CacheDailyMetricRow, CacheStatsRequest, CacheStatsResponse, ExperimentAnalyticsRequest,
    ExperimentAnalyticsResponse, ExperimentRunTrendPoint, ExperimentScoreRunRef,
    ExperimentStatusCount, ExperimentTrialTrendPoint, LearningCandidateListRequest,
    LearningCandidateListResponse, SessionSearchRequest, SessionSearchResponse,
    SessionSearchResult, SessionStatsRequest, SessionStatsResponse, TenantStatsRequest,
    TenantStatsResponse, ToolStatsRequest, ToolStatsResponse, ToolStatsRow,
};
use moa_core::{
    CacheDailyMetric, EventFilter, EventRecord, MoaError, SessionAnalyticsSummary,
    TenantAnalyticsSummary, TenantId, ToolCallSummary,
};
use sqlx::Row;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Minimum spacing between analytics materialized-view refreshes, process-wide.
const MV_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);

static LAST_MV_REFRESH: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn last_mv_refresh() -> &'static Mutex<Option<Instant>> {
    LAST_MV_REFRESH.get_or_init(|| Mutex::new(None))
}

use super::{
    AppState, RouteTranslation, authenticate_direct_request, moa_error_response, parse_json_body,
    parse_json_body_with_tenant, require_direct_authz, route_error,
    translate_json_object_with_tenant_id,
};

/// Handles direct session analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_session_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/session-stats").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: SessionStatsRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Session,
        request.session_id,
        Relation::Participant,
    )
    .await
    {
        return response;
    }
    match moa_session::analytics::get_session_summary(
        &state.pool,
        state.config.database.schema.as_deref(),
        request.session_id,
    )
    .await
    {
        Ok(summary) => Json(session_stats_response_from_summary(summary)).into_response(),
        Err(error) => moa_error_response(error),
    }
}

/// Handles direct tenant analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_tenant_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/tenant-stats").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: TenantStatsRequest = match parse_json_body_with_tenant(&body, identity.tenant_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    maybe_refresh_analytics_materialized_views(&state);
    match moa_session::analytics::get_tenant_stats(
        &state.pool,
        state.config.database.schema.as_deref(),
        &request.tenant_id,
        request.days,
    )
    .await
    {
        Ok(summary) => Json(tenant_stats_response_from_summary(summary)).into_response(),
        Err(error) => moa_error_response(error),
    }
}

/// Handles direct per-tool analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_tool_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/tool-stats").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let mut request: ToolStatsRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if identity.identity_type != IdentityType::Service {
        request.tenant_id = Some(identity.tenant_id);
    }
    let tenant_id = match request.tenant_id {
        Some(tenant_id) => {
            if let Err(response) = require_direct_authz(
                &state,
                &identity,
                ObjectType::Tenant,
                tenant_id,
                Relation::Operator,
            )
            .await
            {
                return response;
            }
            Some(tenant_id)
        }
        None => {
            if identity.identity_type != IdentityType::Service {
                return (
                    StatusCode::FORBIDDEN,
                    "deployment-wide tool stats require a service identity",
                )
                    .into_response();
            }
            if let Err(response) = require_direct_authz(
                &state,
                &identity,
                ObjectType::Tenant,
                identity.tenant_id,
                Relation::Admin,
            )
            .await
            {
                return response;
            }
            None
        }
    };
    match moa_session::analytics::list_tool_call_summaries(
        &state.pool,
        state.config.database.schema.as_deref(),
        tenant_id.as_ref(),
    )
    .await
    {
        Ok(rows) => Json(tool_stats_response_from_rows(tenant_id, rows)).into_response(),
        Err(error) => moa_error_response(error),
    }
}

/// Handles direct tenant cache analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_cache_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/cache-stats").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: CacheStatsRequest = match parse_json_body_with_tenant(&body, identity.tenant_id) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    maybe_refresh_analytics_materialized_views(&state);
    let summary = match moa_session::analytics::get_tenant_stats(
        &state.pool,
        state.config.database.schema.as_deref(),
        &request.tenant_id,
        request.days,
    )
    .await
    {
        Ok(summary) => summary,
        Err(error) => return moa_error_response(error),
    };
    match moa_session::analytics::list_cache_daily_metrics(
        &state.pool,
        state.config.database.schema.as_deref(),
        &request.tenant_id,
        request.days,
    )
    .await
    {
        Ok(daily) => Json(cache_stats_response_from_parts(summary, daily)).into_response(),
        Err(error) => moa_error_response(error),
    }
}

/// Handles direct experiment analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_experiment_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/experiment-stats").await
        {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: ExperimentAnalyticsRequest =
        match parse_json_body_with_tenant(&body, identity.tenant_id) {
            Ok(request) => request,
            Err(response) => return response,
        };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match experiment_stats_inner(&state.pool, request).await {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

/// Handles direct learning-candidate analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_learning_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/learning-candidates")
            .await
        {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: LearningCandidateListRequest =
        match parse_json_body_with_tenant(&body, identity.tenant_id) {
            Ok(request) => request,
            Err(response) => return response,
        };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match moa_session::analytics::list_learning_candidate_summaries(
        &state.pool,
        state.config.database.schema.as_deref(),
        request.tenant_id,
        request.status,
        request.limit,
    )
    .await
    {
        Ok(candidates) => Json(LearningCandidateListResponse {
            tenant_id: request.tenant_id,
            candidates,
        })
        .into_response(),
        Err(error) => moa_error_response(error),
    }
}

/// Handles direct tenant session-search analytics reads at the edge.
#[tracing::instrument(skip(state, headers, body))]
pub async fn handle_session_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/analytics/session-search").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    let request: SessionSearchRequest = match parse_json_body_with_tenant(&body, identity.tenant_id)
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        request.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    let events = match state
        .session_store
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
    {
        Ok(events) => events,
        Err(error) => return moa_error_response(error),
    };
    Json(session_search_response_from_events(request, events)).into_response()
}

/// Trigger an analytics materialized-view refresh at most once per interval.
///
/// The refresh runs in the background so the current request never waits for it,
/// and the timestamp is stamped before spawning so concurrent requests do not
/// each fire a refresh (single-flight). Queries proceed against the current
/// materialized-view state; a failed refresh is logged, not surfaced.
fn maybe_refresh_analytics_materialized_views(state: &AppState) {
    let due = {
        let mut guard = last_mv_refresh()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *guard {
            Some(last) if last.elapsed() < MV_REFRESH_MIN_INTERVAL => false,
            _ => {
                *guard = Some(Instant::now());
                true
            }
        }
    };
    if !due {
        return;
    }
    let store = state.session_store.clone();
    tokio::spawn(async move {
        if let Err(error) = store.refresh_analytics_materialized_views().await {
            tracing::warn!(error = %error, "analytics materialized view refresh failed");
        }
    });
}

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method != Method::POST {
        return None;
    }

    let translation = match uri.path() {
        "/v1/experiments/generate-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/generate_plan", tenant_id)
        }
        "/v1/experiments/run-plan" => {
            translate_json_object_with_tenant_id(body, "/Experiments/run", tenant_id)
        }
        "/v1/experiments/status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/status", tenant_id)
        }
        "/v1/experiments/list" => {
            translate_json_object_with_tenant_id(body, "/Experiments/list", tenant_id)
        }
        "/v1/experiments/trials" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trials", tenant_id)
        }
        "/v1/experiments/trial-status" => {
            translate_json_object_with_tenant_id(body, "/Experiments/trial_status", tenant_id)
        }
        "/v1/experiments/cancel" => {
            translate_json_object_with_tenant_id(body, "/Experiments/cancel", tenant_id)
        }
        "/v1/experiments/propose-improvements" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/propose_improvements",
            tenant_id,
        ),
        "/v1/experiments/scores" => {
            translate_json_object_with_tenant_id(body, "/Experiments/scores", tenant_id)
        }
        "/v1/experiments/compare" => {
            translate_json_object_with_tenant_id(body, "/Experiments/compare", tenant_id)
        }
        "/v1/experiments/agent-revision-simulations" => translate_json_object_with_tenant_id(
            body,
            "/Experiments/run_agent_revision_simulation",
            tenant_id,
        ),
        "/v1/experiments/agent-revision-simulations/compare" => {
            translate_json_object_with_tenant_id(
                body,
                "/Experiments/compare_agent_revision_simulation",
                tenant_id,
            )
        }
        "/v1/admin-maintenance/vector/promote" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/promote_tenant_vector",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/rollback-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/rollback_promotion",
            tenant_id,
        ),
        "/v1/admin-maintenance/vector/finalize-promotion" => translate_json_object_with_tenant_id(
            body,
            "/AdminMaintenance/finalize_promotion",
            tenant_id,
        ),
        "/v1/admin-maintenance/checkpoints/create" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_create".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/list" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_list".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/rollback" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_rollback".to_string(),
            body: body.to_vec(),
        },
        "/v1/admin-maintenance/checkpoints/cleanup" => RouteTranslation::Forward {
            method: Method::POST,
            path: "/AdminMaintenance/checkpoint_cleanup".to_string(),
            body: body.to_vec(),
        },
        "/v1/privacy/export" => {
            translate_json_object_with_tenant_id(body, "/Privacy/export", tenant_id)
        }
        "/v1/privacy/erase" => {
            translate_json_object_with_tenant_id(body, "/Privacy/erase", tenant_id)
        }
        _ => return None,
    };
    Some(translation)
}

fn session_stats_response_from_summary(summary: SessionAnalyticsSummary) -> SessionStatsResponse {
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

fn tenant_stats_response_from_summary(summary: TenantAnalyticsSummary) -> TenantStatsResponse {
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

fn tool_stats_response_from_rows(
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

fn cache_stats_response_from_parts(
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

async fn experiment_stats_inner(
    pool: &sqlx::PgPool,
    request: ExperimentAnalyticsRequest,
) -> Result<ExperimentAnalyticsResponse, Response> {
    let mut conn =
        moa_db::ScopedConn::begin(pool, &moa_core::RlsContext::tenant(request.tenant_id))
            .await
            .map_err(moa_error_response)?;
    let status_rows = sqlx::query(
        r#"
        SELECT status, COUNT(*)::BIGINT AS count
        FROM moa.experiment_run
        WHERE scope = 'tenant'
          AND storage_partition_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY status
        ORDER BY status
        "#,
    )
    .bind(request.tenant_id.to_string())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error_response)?;
    let score_rows = sqlx::query(
        r#"
        SELECT run_uid, name, status, score_run_id, created_at
        FROM moa.experiment_run
        WHERE scope = 'tenant'
          AND storage_partition_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        ORDER BY created_at DESC, run_uid ASC
        LIMIT $4
        "#,
    )
    .bind(request.tenant_id.to_string())
    .bind(request.from_time)
    .bind(request.to_time)
    .bind(i64::from(request.limit))
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error_response)?;
    let run_trend_rows = sqlx::query(
        r#"
        SELECT date_trunc('day', created_at) AS day,
               status,
               COUNT(*)::BIGINT AS count
        FROM moa.experiment_run
        WHERE scope = 'tenant'
          AND storage_partition_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY day, status
        ORDER BY day ASC, status
        "#,
    )
    .bind(request.tenant_id.to_string())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error_response)?;
    let trial_trend_rows = sqlx::query(
        r#"
        SELECT date_trunc('day', created_at) AS day,
               status,
               variant_key,
               scenario_id,
               COUNT(*)::BIGINT AS count
        FROM moa.experiment_trial
        WHERE scope = 'tenant'
          AND storage_partition_id = $1
          AND user_id IS NULL
          AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
          AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
        GROUP BY day, status, variant_key, scenario_id
        ORDER BY day ASC, status, variant_key, scenario_id ASC NULLS FIRST
        "#,
    )
    .bind(request.tenant_id.to_string())
    .bind(request.from_time)
    .bind(request.to_time)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error_response)?;
    conn.commit().await.map_err(moa_error_response)?;

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

    let total_runs = statuses.iter().map(|row| row.count).sum();
    Ok(ExperimentAnalyticsResponse {
        tenant_id: request.tenant_id,
        total_runs,
        statuses,
        score_runs,
        run_trends,
        trial_trends,
    })
}

fn experiment_status_count_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExperimentStatusCount, Response> {
    Ok(ExperimentStatusCount {
        status: row.try_get("status").map_err(sqlx_error_response)?,
        count: u64_from_i64(row.try_get("count").map_err(sqlx_error_response)?, "count")?,
    })
}

fn experiment_score_run_ref_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExperimentScoreRunRef, Response> {
    Ok(ExperimentScoreRunRef {
        run_uid: row.try_get("run_uid").map_err(sqlx_error_response)?,
        name: row.try_get("name").map_err(sqlx_error_response)?,
        status: row.try_get("status").map_err(sqlx_error_response)?,
        score_run_id: row.try_get("score_run_id").map_err(sqlx_error_response)?,
        created_at: row.try_get("created_at").map_err(sqlx_error_response)?,
    })
}

fn experiment_run_trend_point_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExperimentRunTrendPoint, Response> {
    Ok(ExperimentRunTrendPoint {
        day: row.try_get("day").map_err(sqlx_error_response)?,
        status: row.try_get("status").map_err(sqlx_error_response)?,
        count: u64_from_i64(row.try_get("count").map_err(sqlx_error_response)?, "count")?,
    })
}

fn experiment_trial_trend_point_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExperimentTrialTrendPoint, Response> {
    Ok(ExperimentTrialTrendPoint {
        day: row.try_get("day").map_err(sqlx_error_response)?,
        status: row.try_get("status").map_err(sqlx_error_response)?,
        variant_key: row.try_get("variant_key").map_err(sqlx_error_response)?,
        scenario_id: row.try_get("scenario_id").map_err(sqlx_error_response)?,
        count: u64_from_i64(row.try_get("count").map_err(sqlx_error_response)?, "count")?,
    })
}

fn u64_from_i64(value: i64, field: &'static str) -> Result<u64, Response> {
    u64::try_from(value).map_err(|_| route_error(format!("{field} was negative: {value}")))
}

fn session_search_response_from_events(
    request: SessionSearchRequest,
    events: Vec<EventRecord>,
) -> SessionSearchResponse {
    SessionSearchResponse {
        tenant_id: request.tenant_id,
        query: request.query,
        results: events
            .iter()
            .map(|event| SessionSearchResult {
                session_id: event.session_id,
                event_id: event.id,
                sequence_num: event.sequence_num,
                event_type: event.event_type,
                timestamp: event.timestamp,
                snippet: event.event.type_name().to_string(),
            })
            .collect(),
    }
}

fn sqlx_error_response(error: sqlx::Error) -> Response {
    moa_error_response(MoaError::StorageError(error.to_string()))
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{Method, Uri};

    use crate::routes::RouteTranslation;
    use crate::routes::test_support::{test_tenant_json, translate};

    #[test]
    fn analytics_public_routes_do_not_translate_to_restate_handlers() {
        // Pins: hosted analytics read routes are direct edge handlers, not Restate forwards.
        let paths = [
            "/v1/analytics/session-stats",
            "/v1/analytics/tenant-stats",
            "/v1/analytics/tool-stats",
            "/v1/analytics/cache-stats",
            "/v1/analytics/experiment-stats",
            "/v1/analytics/learning-candidates",
            "/v1/analytics/session-search",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through direct routing, got: {message}")
                }
            }
        }
    }

    #[test]
    fn eval_public_routes_do_not_translate_to_product_handlers() {
        // Pins: hosted eval is not part of the default public product edge surface.
        let paths = [
            "/v1/evals/plan",
            "/v1/evals/suites/list",
            "/v1/evals/run",
            "/v1/evals/run-status",
            "/v1/evals/datasets/register",
            "/v1/evals/datasets/list",
            "/v1/evals/replay",
            "/v1/evals/scores",
            "/v1/evals/compare",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward {
                    method,
                    path,
                    body: _,
                } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through unchanged, got: {message}")
                }
            }
        }
    }

    #[test]
    fn experiments_public_routes_translate_to_restate_handlers() {
        // Pins: hosted experiment edge routes forward to the internal Experiments service paths.
        let cases = [
            (
                "/v1/experiments/generate-plan",
                "/Experiments/generate_plan",
            ),
            ("/v1/experiments/run-plan", "/Experiments/run"),
            ("/v1/experiments/status", "/Experiments/status"),
            ("/v1/experiments/list", "/Experiments/list"),
            ("/v1/experiments/trials", "/Experiments/trials"),
            ("/v1/experiments/trial-status", "/Experiments/trial_status"),
            ("/v1/experiments/cancel", "/Experiments/cancel"),
            (
                "/v1/experiments/propose-improvements",
                "/Experiments/propose_improvements",
            ),
            ("/v1/experiments/scores", "/Experiments/scores"),
            ("/v1/experiments/compare", "/Experiments/compare"),
            (
                "/v1/experiments/agent-revision-simulations",
                "/Experiments/run_agent_revision_simulation",
            ),
            (
                "/v1/experiments/agent-revision-simulations/compare",
                "/Experiments/compare_agent_revision_simulation",
            ),
        ];

        for (public_path, internal_path) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded.get("tenant_id"), Some(&test_tenant_json()));
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn admin_maintenance_public_routes_translate_to_restate_handlers() {
        // Pins: hosted admin-maintenance routes forward to the internal AdminMaintenance service paths.
        let cases = [
            (
                "/v1/admin-maintenance/vector/promote",
                "/AdminMaintenance/promote_tenant_vector",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "target_backend": "turbopuffer",
                    "validate_percent": 5,
                    "dual_read_hours": 24
                }),
            ),
            (
                "/v1/admin-maintenance/vector/rollback-promotion",
                "/AdminMaintenance/rollback_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "rollback"
                }),
            ),
            (
                "/v1/admin-maintenance/vector/finalize-promotion",
                "/AdminMaintenance/finalize_promotion",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "action": "finalize"
                }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/create",
                "/AdminMaintenance/checkpoint_create",
                serde_json::json!({ "label": "before-deploy", "session_id": null }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/list",
                "/AdminMaintenance/checkpoint_list",
                serde_json::json!({}),
            ),
            (
                "/v1/admin-maintenance/checkpoints/rollback",
                "/AdminMaintenance/checkpoint_rollback",
                serde_json::json!({ "id": "br-checkpoint" }),
            ),
            (
                "/v1/admin-maintenance/checkpoints/cleanup",
                "/AdminMaintenance/checkpoint_cleanup",
                serde_json::json!({}),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            if public_path.contains("/vector/")
                && let Some(object) = input_body.as_object_mut()
            {
                object.remove("tenant_id");
            }
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    if public_path.contains("/vector/") {
                        assert_eq!(forwarded, expected_body, "{public_path} body changed");
                    } else {
                        assert_eq!(
                            forwarded, input_body,
                            "{public_path} body should pass through"
                        );
                    }
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn lineage_public_routes_do_not_translate_to_restate_handlers() {
        // Pins: hosted lineage routes stay on direct edge handlers.
        let paths = [
            "/v1/lineage/explain",
            "/v1/lineage/query",
            "/v1/lineage/verify",
        ];

        for public_path in paths {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let body = Bytes::from_static(br#"{}"#);

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::NoChange => {}
                RouteTranslation::Forward { method, path, .. } => {
                    panic!("{public_path} must not translate, got {method} {path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should fall through direct routing, got: {message}")
                }
            }
        }
    }

    #[test]
    fn privacy_public_routes_translate_to_restate_handlers() {
        // Pins: privacy operations are still durable Restate service calls.
        let cases = [
            (
                "/v1/privacy/export",
                "/Privacy/export",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
            (
                "/v1/privacy/erase",
                "/Privacy/erase",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "subject_user_id": "22222222-2222-2222-2222-222222222222",
                    "reason": "GDPR",
                    "approval_token": "token"
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.remove("tenant_id");
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }
}
