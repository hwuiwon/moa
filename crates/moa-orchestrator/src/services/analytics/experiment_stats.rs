//! Experiment analytics SQL orchestration and response assembly.

use moa_core::wire::analytics::{
    ExperimentAnalyticsRequest, ExperimentAnalyticsResponse, ExperimentRunTrendPoint,
    ExperimentScoreRunRef, ExperimentStatusCount, ExperimentTrialTrendPoint,
};
use moa_core::{MoaError, TenantId};
use moa_db::ScopedConn;
use moa_memory_types::ScopeContext;
use restate_sdk::prelude::*;
use sqlx::{Row, postgres::PgRow};

use super::to_handler_error;

/// Loads experiment analytics rows and converts them to the public response.
pub(super) async fn experiment_stats_inner(
    pool: sqlx::PgPool,
    request: ExperimentAnalyticsRequest,
) -> Result<ExperimentAnalyticsResponse, HandlerError> {
    let mut conn = ScopedConn::begin(&pool, &ScopeContext::tenant(request.tenant_id))
        .await
        .map_err(to_handler_error)?;
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
    .map_err(sqlx_to_handler_error)?;
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
    .map_err(sqlx_to_handler_error)?;
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
    .map_err(sqlx_to_handler_error)?;
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
        ORDER BY day ASC,
                 status,
                 variant_key,
                 scenario_id ASC NULLS FIRST
        "#,
    )
    .bind(request.tenant_id.to_string())
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
        request.tenant_id,
        statuses,
        score_runs,
        run_trends,
        trial_trends,
    ))
}

/// Converts experiment status counts and score-run references into a response.
#[must_use]
pub fn experiment_stats_response_from_parts(
    tenant_id: TenantId,
    statuses: Vec<ExperimentStatusCount>,
    score_runs: Vec<ExperimentScoreRunRef>,
    run_trends: Vec<ExperimentRunTrendPoint>,
    trial_trends: Vec<ExperimentTrialTrendPoint>,
) -> ExperimentAnalyticsResponse {
    let total_runs = statuses.iter().map(|row| row.count).sum();
    ExperimentAnalyticsResponse {
        tenant_id,
        total_runs,
        statuses,
        score_runs,
        run_trends,
        trial_trends,
    }
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

fn u64_from_i64(value: i64, field: &'static str) -> Result<u64, HandlerError> {
    u64::try_from(value)
        .map_err(|_| TerminalError::new(format!("{field} was negative: {value}")).into())
}

fn sqlx_to_handler_error(error: sqlx::Error) -> HandlerError {
    to_handler_error(MoaError::StorageError(error.to_string()))
}
