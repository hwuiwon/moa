//! Typed analytics reads over session summary, tool summary, and rollup views.

use chrono::{DateTime, Duration, NaiveTime, Utc};
use sqlx::{PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{MoaError, Result, SessionId, SessionStatus, UserId, WorkspaceId};

/// One session-level analytics row sourced from the `session_summary` view.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionAnalyticsSummary {
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Current persisted session status.
    pub status: SessionStatus,
    /// Number of completed assistant turns.
    pub turn_count: u64,
    /// Total event count for the session.
    pub event_count: u64,
    /// Total input tokens across cached and uncached paths.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Fraction of input tokens served from cache.
    pub cache_hit_rate: f64,
    /// Session wall-clock duration in seconds.
    pub duration_seconds: f64,
    /// Number of tool calls recorded for the session.
    pub tool_call_count: u64,
    /// Number of error events recorded for the session.
    pub error_count: u64,
}

/// One per-tool analytics row sourced from `tool_call_summary` or `tool_call_analytics`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallSummary {
    /// Stable tool name.
    pub tool_name: String,
    /// Number of completed calls for the tool.
    pub call_count: u64,
    /// Mean duration in milliseconds.
    pub avg_duration_ms: f64,
    /// Median duration in milliseconds.
    pub p50_ms: f64,
    /// P95 duration in milliseconds.
    pub p95_ms: f64,
    /// Fraction of calls that succeeded.
    pub success_rate: f64,
}

/// One per-turn analytics row sourced from the `session_turn_metrics` materialized view.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionTurnMetric {
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// One-based turn number within the session.
    pub turn_number: u64,
    /// Timestamp when the assistant turn completed.
    pub finished_at: DateTime<Utc>,
    /// Model recorded for the turn.
    pub model: String,
    /// Pipeline duration when available.
    pub pipeline_ms: Option<f64>,
    /// Provider response duration in milliseconds.
    pub llm_ms: f64,
    /// Aggregate tool execution duration in milliseconds for the turn.
    pub tool_ms: f64,
    /// Number of tool calls in the turn.
    pub tool_call_count: u64,
    /// Uncached input tokens for the turn.
    pub input_tokens_uncached: u64,
    /// Cache-write input tokens for the turn.
    pub input_tokens_cache_write: u64,
    /// Cache-read input tokens for the turn.
    pub input_tokens_cache_read: u64,
    /// Total input tokens for the turn.
    pub total_input_tokens: u64,
    /// Output tokens for the turn.
    pub output_tokens: u64,
    /// Turn cost in cents.
    pub cost_cents: u64,
}

/// Aggregate workspace metrics over a bounded recent time window.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceAnalyticsSummary {
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Number of whole days included in the rollup window.
    pub days: u32,
    /// Session count across the window.
    pub session_count: u64,
    /// Turn count across the window.
    pub turn_count: u64,
    /// Total input tokens across the window.
    pub total_input_tokens: u64,
    /// Cache-read input tokens across the window.
    pub total_cache_read_tokens: u64,
    /// Total output tokens across the window.
    pub total_output_tokens: u64,
    /// Total cost in cents across the window.
    pub total_cost_cents: u64,
    /// Weighted cache-hit rate for the window.
    pub cache_hit_rate: f64,
}

/// One daily cache trend point sourced from `daily_workspace_metrics`.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheDailyMetric {
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// UTC day bucket.
    pub day: DateTime<Utc>,
    /// Session count on the day.
    pub session_count: u64,
    /// Turn count on the day.
    pub turn_count: u64,
    /// Total input tokens on the day.
    pub total_input_tokens: u64,
    /// Total cache-read tokens on the day.
    pub total_cache_read_tokens: u64,
    /// Total output tokens on the day.
    pub total_output_tokens: u64,
    /// Total cost in cents on the day.
    pub total_cost_cents: u64,
    /// Average cache-hit rate on the day.
    pub avg_cache_hit_rate: f64,
}

/// Loads one session summary row by session id.
pub async fn get_session_summary(
    pool: &PgPool,
    schema_name: Option<&str>,
    session_id: SessionId,
) -> Result<SessionAnalyticsSummary> {
    let session_summary = qualified_relation(schema_name, "session_summary");
    let row = sqlx::query(&format!(
        "SELECT \
             id, workspace_id, user_id, status, turn_count, event_count, \
             total_input_tokens, total_output_tokens, total_cost_cents, \
             cache_hit_rate, duration_seconds, tool_call_count, error_count \
         FROM {session_summary} \
         WHERE id = $1 \
         LIMIT 1"
    ))
    .bind(session_id.0)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?
    .ok_or_else(|| MoaError::SessionNotFound(session_id))?;

    session_analytics_from_row(&row)
}

/// Lists per-tool summary rows, optionally restricted to one workspace.
pub async fn list_tool_call_summaries(
    pool: &PgPool,
    schema_name: Option<&str>,
    workspace_id: Option<&WorkspaceId>,
) -> Result<Vec<ToolCallSummary>> {
    let query = match workspace_id {
        Some(_) => {
            let tool_call_analytics = qualified_relation(schema_name, "tool_call_analytics");
            format!(
                "SELECT \
                     tool_name, \
                     COUNT(*)::BIGINT AS call_count, \
                     AVG(duration_ms)::DOUBLE PRECISION AS avg_duration_ms, \
                     PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms, \
                     PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_ms, \
                     AVG(CASE WHEN success THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS success_rate \
                 FROM {tool_call_analytics} \
                 WHERE finished_at IS NOT NULL AND workspace_id = $1 \
                 GROUP BY tool_name \
                 ORDER BY call_count DESC, p95_ms DESC, tool_name ASC"
            )
        }
        None => {
            let tool_call_summary = qualified_relation(schema_name, "tool_call_summary");
            format!(
                "SELECT \
                     tool_name, call_count, avg_duration_ms, p50_ms, p95_ms, success_rate \
                 FROM {tool_call_summary} \
                 ORDER BY call_count DESC, p95_ms DESC, tool_name ASC"
            )
        }
    };

    let rows = match workspace_id {
        Some(workspace_id) => {
            sqlx::query(&query)
                .bind(workspace_id.to_string())
                .fetch_all(pool)
                .await
        }
        None => sqlx::query(&query).fetch_all(pool).await,
    }
    .map_err(map_sqlx_error)?;

    rows.iter().map(tool_call_summary_from_row).collect()
}

/// Lists per-turn rows for one session from the `session_turn_metrics` materialized view.
pub async fn list_session_turn_metrics(
    pool: &PgPool,
    schema_name: Option<&str>,
    session_id: SessionId,
) -> Result<Vec<SessionTurnMetric>> {
    let session_turn_metrics = qualified_relation(schema_name, "session_turn_metrics");
    let rows = sqlx::query(&format!(
        "SELECT \
             session_id, workspace_id, user_id, turn_number, finished_at, model, \
             pipeline_ms, llm_ms, tool_ms, tool_call_count, input_tokens_uncached, \
             input_tokens_cache_write, input_tokens_cache_read, total_input_tokens, \
             output_tokens, cost_cents \
         FROM {session_turn_metrics} \
         WHERE session_id = $1 \
         ORDER BY turn_number ASC"
    ))
    .bind(session_id.0)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    rows.iter().map(session_turn_metric_from_row).collect()
}

/// Loads a recent workspace rollup from `daily_workspace_metrics`.
pub async fn get_workspace_stats(
    pool: &PgPool,
    schema_name: Option<&str>,
    workspace_id: &WorkspaceId,
    days: u32,
) -> Result<WorkspaceAnalyticsSummary> {
    let daily_workspace_metrics = qualified_relation(schema_name, "daily_workspace_metrics");
    let start_day = analytics_window_start(days);
    let row = sqlx::query(&format!(
        "SELECT \
             COALESCE(SUM(session_count), 0)::BIGINT AS session_count, \
             COALESCE(SUM(turn_count), 0)::BIGINT AS turn_count, \
             COALESCE(SUM(total_input_tokens), 0)::BIGINT AS total_input_tokens, \
             COALESCE(SUM(total_cache_read_tokens), 0)::BIGINT AS total_cache_read_tokens, \
             COALESCE(SUM(total_output_tokens), 0)::BIGINT AS total_output_tokens, \
             COALESCE(SUM(total_cost_cents), 0)::BIGINT AS total_cost_cents, \
             CASE \
                 WHEN COALESCE(SUM(total_input_tokens), 0) = 0 THEN 0.0 \
                 ELSE COALESCE(SUM(total_cache_read_tokens), 0)::DOUBLE PRECISION \
                     / COALESCE(SUM(total_input_tokens), 0)::DOUBLE PRECISION \
             END AS cache_hit_rate \
         FROM {daily_workspace_metrics} \
         WHERE workspace_id = $1 AND day >= $2"
    ))
    .bind(workspace_id.to_string())
    .bind(start_day)
    .fetch_one(pool)
    .await
    .map_err(map_sqlx_error)?;

    Ok(WorkspaceAnalyticsSummary {
        workspace_id: workspace_id.clone(),
        days: normalized_days(days),
        session_count: row
            .try_get::<i64, _>("session_count")
            .map_err(map_sqlx_error)? as u64,
        turn_count: row
            .try_get::<i64, _>("turn_count")
            .map_err(map_sqlx_error)? as u64,
        total_input_tokens: row
            .try_get::<i64, _>("total_input_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_cache_read_tokens: row
            .try_get::<i64, _>("total_cache_read_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_output_tokens: row
            .try_get::<i64, _>("total_output_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_cost_cents: row
            .try_get::<i64, _>("total_cost_cents")
            .map_err(map_sqlx_error)? as u64,
        cache_hit_rate: row
            .try_get::<f64, _>("cache_hit_rate")
            .map_err(map_sqlx_error)?,
    })
}

/// Lists daily cache metrics for one workspace over a recent window.
pub async fn list_cache_daily_metrics(
    pool: &PgPool,
    schema_name: Option<&str>,
    workspace_id: &WorkspaceId,
    days: u32,
) -> Result<Vec<CacheDailyMetric>> {
    let daily_workspace_metrics = qualified_relation(schema_name, "daily_workspace_metrics");
    let start_day = analytics_window_start(days);
    let rows = sqlx::query(&format!(
        "SELECT \
             workspace_id, day, session_count, turn_count, total_input_tokens, \
             total_cache_read_tokens, total_output_tokens, total_cost_cents, avg_cache_hit_rate \
         FROM {daily_workspace_metrics} \
         WHERE workspace_id = $1 AND day >= $2 \
         ORDER BY day ASC"
    ))
    .bind(workspace_id.to_string())
    .bind(start_day)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    rows.iter().map(cache_daily_metric_from_row).collect()
}

fn analytics_window_start(days: u32) -> DateTime<Utc> {
    let start_of_today = Utc::now().date_naive().and_time(NaiveTime::MIN).and_utc();
    let days = i64::from(normalized_days(days).saturating_sub(1));
    start_of_today - Duration::days(days)
}

fn normalized_days(days: u32) -> u32 {
    days.max(1)
}

fn session_analytics_from_row(row: &PgRow) -> Result<SessionAnalyticsSummary> {
    Ok(SessionAnalyticsSummary {
        session_id: SessionId(row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?),
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: UserId(
            row.try_get::<String, _>("user_id")
                .map_err(map_sqlx_error)?,
        ),
        status: session_status_from_db(
            &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
        )?,
        turn_count: row
            .try_get::<i64, _>("turn_count")
            .map_err(map_sqlx_error)? as u64,
        event_count: row
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as u64,
        total_input_tokens: row
            .try_get::<i64, _>("total_input_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_output_tokens: row
            .try_get::<i64, _>("total_output_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_cost_cents: row
            .try_get::<i64, _>("total_cost_cents")
            .map_err(map_sqlx_error)? as u64,
        cache_hit_rate: row
            .try_get::<f64, _>("cache_hit_rate")
            .map_err(map_sqlx_error)?,
        duration_seconds: row
            .try_get::<f64, _>("duration_seconds")
            .map_err(map_sqlx_error)?,
        tool_call_count: row
            .try_get::<i64, _>("tool_call_count")
            .map_err(map_sqlx_error)? as u64,
        error_count: row
            .try_get::<i64, _>("error_count")
            .map_err(map_sqlx_error)? as u64,
    })
}

fn tool_call_summary_from_row(row: &PgRow) -> Result<ToolCallSummary> {
    Ok(ToolCallSummary {
        tool_name: row
            .try_get::<String, _>("tool_name")
            .map_err(map_sqlx_error)?,
        call_count: row
            .try_get::<i64, _>("call_count")
            .map_err(map_sqlx_error)? as u64,
        avg_duration_ms: row
            .try_get::<Option<f64>, _>("avg_duration_ms")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        p50_ms: row
            .try_get::<Option<f64>, _>("p50_ms")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        p95_ms: row
            .try_get::<Option<f64>, _>("p95_ms")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        success_rate: row
            .try_get::<Option<f64>, _>("success_rate")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
    })
}

fn session_turn_metric_from_row(row: &PgRow) -> Result<SessionTurnMetric> {
    Ok(SessionTurnMetric {
        session_id: SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        ),
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: UserId(
            row.try_get::<String, _>("user_id")
                .map_err(map_sqlx_error)?,
        ),
        turn_number: row
            .try_get::<i64, _>("turn_number")
            .map_err(map_sqlx_error)? as u64,
        finished_at: row
            .try_get::<DateTime<Utc>, _>("finished_at")
            .map_err(map_sqlx_error)?,
        model: row.try_get::<String, _>("model").map_err(map_sqlx_error)?,
        pipeline_ms: row
            .try_get::<Option<f64>, _>("pipeline_ms")
            .map_err(map_sqlx_error)?,
        llm_ms: row.try_get::<f64, _>("llm_ms").map_err(map_sqlx_error)?,
        tool_ms: row.try_get::<f64, _>("tool_ms").map_err(map_sqlx_error)?,
        tool_call_count: row
            .try_get::<i64, _>("tool_call_count")
            .map_err(map_sqlx_error)? as u64,
        input_tokens_uncached: row
            .try_get::<i64, _>("input_tokens_uncached")
            .map_err(map_sqlx_error)? as u64,
        input_tokens_cache_write: row
            .try_get::<i64, _>("input_tokens_cache_write")
            .map_err(map_sqlx_error)? as u64,
        input_tokens_cache_read: row
            .try_get::<i64, _>("input_tokens_cache_read")
            .map_err(map_sqlx_error)? as u64,
        total_input_tokens: row
            .try_get::<i64, _>("total_input_tokens")
            .map_err(map_sqlx_error)? as u64,
        output_tokens: row
            .try_get::<i64, _>("output_tokens")
            .map_err(map_sqlx_error)? as u64,
        cost_cents: row
            .try_get::<i64, _>("cost_cents")
            .map_err(map_sqlx_error)? as u64,
    })
}

fn cache_daily_metric_from_row(row: &PgRow) -> Result<CacheDailyMetric> {
    Ok(CacheDailyMetric {
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        day: row
            .try_get::<DateTime<Utc>, _>("day")
            .map_err(map_sqlx_error)?,
        session_count: row
            .try_get::<i64, _>("session_count")
            .map_err(map_sqlx_error)? as u64,
        turn_count: row
            .try_get::<i64, _>("turn_count")
            .map_err(map_sqlx_error)? as u64,
        total_input_tokens: row
            .try_get::<i64, _>("total_input_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_cache_read_tokens: row
            .try_get::<i64, _>("total_cache_read_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_output_tokens: row
            .try_get::<i64, _>("total_output_tokens")
            .map_err(map_sqlx_error)? as u64,
        total_cost_cents: row
            .try_get::<i64, _>("total_cost_cents")
            .map_err(map_sqlx_error)? as u64,
        avg_cache_hit_rate: row
            .try_get::<f64, _>("avg_cache_hit_rate")
            .map_err(map_sqlx_error)?,
    })
}

fn session_status_from_db(value: &str) -> Result<SessionStatus> {
    match value {
        "created" => Ok(SessionStatus::Created),
        "running" => Ok(SessionStatus::Running),
        "paused" => Ok(SessionStatus::Paused),
        "waiting_approval" => Ok(SessionStatus::WaitingApproval),
        "completed" => Ok(SessionStatus::Completed),
        "cancelled" => Ok(SessionStatus::Cancelled),
        "failed" => Ok(SessionStatus::Failed),
        other => Err(MoaError::StorageError(format!(
            "unknown session status value `{other}`"
        ))),
    }
}

fn qualified_relation(schema_name: Option<&str>, relation_name: &str) -> String {
    match schema_name {
        Some(schema_name) => format!(
            "{}.{}",
            quote_identifier(schema_name),
            quote_identifier(relation_name)
        ),
        None => relation_name.to_string(),
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
