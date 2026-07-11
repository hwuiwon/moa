//! Typed analytics reads over session summary, tool summary, and rollup views.

use chrono::{DateTime, Duration, NaiveTime, Utc};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use moa_core::wire::analytics::LearningCandidateSummary;
use moa_core::{
    analytics::CacheDailyMetric, analytics::SessionAnalyticsSummary, analytics::SessionTurnMetric,
    analytics::TenantAnalyticsSummary, analytics::ToolCallSummary, error::MoaError, error::Result,
    types::contact::ContactId, types::experience::LearningCandidateStatus,
    types::experience::LearningCandidateType, types::experience::LearningRiskClass,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;

use crate::queries::from_db;

/// Loads one session summary row by session id.
pub async fn get_session_summary(
    pool: &PgPool,
    schema_name: Option<&str>,
    session_id: SessionId,
) -> Result<SessionAnalyticsSummary> {
    let session_summary = qualified_relation(schema_name, "session_summary");
    let row = sqlx::query(&format!(
        "SELECT \
             id, tenant_id, contact_id, status, turn_count, event_count, \
             total_input_tokens, total_output_tokens, total_cost_cents, \
             main_cost_cents, auxiliary_cost_cents, \
             cache_hit_rate, duration_seconds, tool_call_count, error_count \
         FROM {session_summary} \
         WHERE id = $1 \
         LIMIT 1"
    ))
    .bind(session_id.0)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?
    .ok_or(MoaError::SessionNotFound(session_id))?;

    session_analytics_from_row(&row)
}

/// Lists per-tool summary rows, optionally restricted to one tenant.
pub async fn list_tool_call_summaries(
    pool: &PgPool,
    schema_name: Option<&str>,
    tenant_id: Option<&TenantId>,
) -> Result<Vec<ToolCallSummary>> {
    let query = match tenant_id {
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
                 WHERE finished_at IS NOT NULL AND storage_partition_id = $1 \
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

    let rows = match tenant_id {
        Some(tenant_id) => {
            sqlx::query(&query)
                .bind(tenant_id.to_string())
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
             session_id, tenant_id, contact_id, turn_number, finished_at, model, \
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

/// Loads a recent tenant rollup from `daily_storage_partition_metrics`.
pub async fn get_tenant_stats(
    pool: &PgPool,
    schema_name: Option<&str>,
    tenant_id: &TenantId,
    days: u32,
) -> Result<TenantAnalyticsSummary> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    let summary = get_tenant_stats_with_conn(conn.as_mut(), schema_name, tenant_id, days).await?;
    conn.commit().await?;
    Ok(summary)
}

async fn get_tenant_stats_with_conn(
    conn: &mut PgConnection,
    schema_name: Option<&str>,
    tenant_id: &TenantId,
    days: u32,
) -> Result<TenantAnalyticsSummary> {
    let daily_storage_partition_metrics =
        qualified_relation(schema_name, "daily_storage_partition_metrics");
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
         FROM {daily_storage_partition_metrics} \
         WHERE storage_partition_id = $1 AND day >= $2"
    ))
    .bind(tenant_id.to_string())
    .bind(start_day)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    Ok(TenantAnalyticsSummary {
        tenant_id: *tenant_id,
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

/// Lists daily cache metrics for one tenant over a recent window.
pub async fn list_cache_daily_metrics(
    pool: &PgPool,
    schema_name: Option<&str>,
    tenant_id: &TenantId,
    days: u32,
) -> Result<Vec<CacheDailyMetric>> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    let rows =
        list_cache_daily_metrics_with_conn(conn.as_mut(), schema_name, tenant_id, days).await?;
    conn.commit().await?;
    Ok(rows)
}

/// Lists redacted learning-candidate summaries for one tenant.
pub async fn list_learning_candidate_summaries(
    pool: &PgPool,
    schema_name: Option<&str>,
    tenant_id: TenantId,
    status: Option<LearningCandidateStatus>,
    limit: u32,
) -> Result<Vec<LearningCandidateSummary>> {
    let learning_candidates = qualified_relation(schema_name, "learning_candidates");
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT id, tenant_id, storage_partition_id, contact_id, candidate_type, status, \
         target_id, target_label, task_fingerprint, payload, \
         confidence::DOUBLE PRECISION AS confidence, risk_class, created_at, updated_at \
         FROM {learning_candidates} WHERE tenant_id = "
    ));
    query.push_bind(tenant_id.0);
    if let Some(status) = status {
        query.push(" AND status = ");
        query.push_bind(status.as_str());
    }
    query.push(" ORDER BY updated_at DESC, id ASC LIMIT ");
    query.push_bind(i64::from(limit));

    let rows = query
        .build()
        .fetch_all(pool)
        .await
        .map_err(map_sqlx_error)?;
    rows.iter()
        .map(learning_candidate_summary_from_row)
        .collect()
}

async fn list_cache_daily_metrics_with_conn(
    conn: &mut PgConnection,
    schema_name: Option<&str>,
    tenant_id: &TenantId,
    days: u32,
) -> Result<Vec<CacheDailyMetric>> {
    let daily_storage_partition_metrics =
        qualified_relation(schema_name, "daily_storage_partition_metrics");
    let start_day = analytics_window_start(days);
    let rows = sqlx::query(&format!(
        "SELECT \
             storage_partition_id, day, session_count, turn_count, total_input_tokens, \
             total_cache_read_tokens, total_output_tokens, total_cost_cents, avg_cache_hit_rate \
         FROM {daily_storage_partition_metrics} \
         WHERE storage_partition_id = $1 AND day >= $2 \
         ORDER BY day ASC"
    ))
    .bind(tenant_id.to_string())
    .bind(start_day)
    .fetch_all(&mut *conn)
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
        tenant_id: TenantId(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        contact_id: row
            .try_get::<Option<Uuid>, _>("contact_id")
            .map_err(map_sqlx_error)?
            .map(ContactId),
        status: from_db(
            "session status",
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
        main_cost_cents: row
            .try_get::<i64, _>("main_cost_cents")
            .map_err(map_sqlx_error)? as u64,
        auxiliary_cost_cents: row
            .try_get::<i64, _>("auxiliary_cost_cents")
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
        tenant_id: TenantId(
            row.try_get::<Uuid, _>("tenant_id")
                .map_err(map_sqlx_error)?,
        ),
        contact_id: row
            .try_get::<Option<Uuid>, _>("contact_id")
            .map_err(map_sqlx_error)?
            .map(ContactId),
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
        tenant_id: TenantId(
            row.try_get::<String, _>("storage_partition_id")
                .map_err(map_sqlx_error)?
                .parse()
                .map_err(|error| {
                    MoaError::StorageError(format!("invalid tenant id in analytics row: {error}"))
                })?,
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

fn learning_candidate_summary_from_row(row: &PgRow) -> Result<LearningCandidateSummary> {
    let candidate_type = from_db::<LearningCandidateType>(
        "learning candidate type",
        &row.try_get::<String, _>("candidate_type")
            .map_err(map_sqlx_error)?,
    )?;
    let status = from_db::<LearningCandidateStatus>(
        "learning candidate status",
        &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
    )?;
    let risk_class = from_db::<LearningRiskClass>(
        "learning risk class",
        &row.try_get::<String, _>("risk_class")
            .map_err(map_sqlx_error)?,
    )?;
    let payload: Value = row.try_get("payload").map_err(map_sqlx_error)?;

    Ok(LearningCandidateSummary {
        id: row.try_get("id").map_err(map_sqlx_error)?,
        tenant_id: row.try_get("tenant_id").map_err(map_sqlx_error)?,
        contact_id: row.try_get("contact_id").map_err(map_sqlx_error)?,
        candidate_type,
        status,
        target_id: row.try_get("target_id").map_err(map_sqlx_error)?,
        target_label: row.try_get("target_label").map_err(map_sqlx_error)?,
        task_fingerprint: row.try_get("task_fingerprint").map_err(map_sqlx_error)?,
        confidence: row.try_get("confidence").map_err(map_sqlx_error)?,
        risk_class,
        payload_preview: redacted_payload_preview(&payload),
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn redacted_payload_preview(value: &Value) -> String {
    let redacted = redact_json_value(value);
    truncate_preview(&redacted.to_string(), 240)
}

fn redact_json_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if is_sensitive_key(key) {
                        (key.clone(), Value::String("[redacted]".to_string()))
                    } else {
                        (key.clone(), redact_json_value(value))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_json_value).collect()),
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    normalized.contains("secret")
        || normalized.contains("token")
        || normalized.contains("password")
        || normalized.contains("credential")
        || normalized.contains("api_key")
}

fn truncate_preview(text: &str, limit: usize) -> String {
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
