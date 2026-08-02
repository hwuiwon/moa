//! Windowed fact export: `turn_fact` and `tool_call_fact` are computed in
//! Postgres by reusing the exact `session_turn_metrics` and
//! `tool_call_analytics` source-view SQL, scoped to the sessions touched by
//! the current events batch, and upserted into ClickHouse `ReplacingMergeTree`
//! tables with a fresh `export_version`. Late tool results re-emit the affected
//! turn rows on a later pass — self-healing, and parity with the Postgres
//! matviews holds by construction because the transform SQL is shared.
//!
//! `turn_number` is read from the authoritative ordinal persisted on each event.
//! `model_tier` is the constant `'main'` the view emits.

use chrono::{DateTime, Utc};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::{AnalyticsExporter, ExportError, FACT_SESSION_CHUNK, record_rows};

/// `turn_fact` recompute: the `session_turn_metrics` CTE filtered to `$1`
/// (session ids) with `$2` stamped as `export_version`.
const TURN_FACT_SQL: &str = "WITH input_sessions AS ( \
        SELECT DISTINCT input.session_id \
        FROM UNNEST($1::uuid[]) AS input(session_id) \
    ), \
    brain_turns AS ( \
        SELECT e.session_id, e.sequence_num AS response_sequence_num, \
            e.turn_number, \
            LAG(e.sequence_num, 1, -1) OVER (PARTITION BY e.session_id ORDER BY e.sequence_num)::BIGINT AS previous_response_sequence_num, \
            e.timestamp AS finished_at, e.payload -> 'data' AS response_data \
        FROM input_sessions input \
        JOIN events e ON e.session_id = input.session_id \
        WHERE e.event_type = 'BrainResponse' \
    ), \
    tool_calls AS ( \
        SELECT e.id, e.session_id, e.sequence_num, e.timestamp, e.payload \
        FROM input_sessions input \
        JOIN events e ON e.session_id = input.session_id \
        WHERE e.event_type = 'ToolCall' \
    ), \
    terminal_events AS ( \
        SELECT DISTINCT ON (e.session_id, e.event_type, e.payload -> 'data' ->> 'tool_id') \
            e.id, e.session_id, e.event_type, e.payload, e.timestamp, \
            e.payload -> 'data' ->> 'tool_id' AS tool_id \
        FROM input_sessions input \
        JOIN events e ON e.session_id = input.session_id \
        WHERE e.event_type IN ('ToolResult', 'ToolError') \
        ORDER BY e.session_id, e.event_type, e.payload -> 'data' ->> 'tool_id', e.sequence_num \
    ), \
    tool_metrics AS ( \
        SELECT bt.session_id, bt.turn_number, COUNT(tc.id)::BIGINT AS tool_call_count, \
            COALESCE(SUM(CASE \
                WHEN tr.id IS NOT NULL THEN COALESCE((tr.payload -> 'data' ->> 'duration_ms')::DOUBLE PRECISION, EXTRACT(EPOCH FROM (tr.timestamp - tc.timestamp)) * 1000.0) \
                WHEN te.id IS NOT NULL THEN EXTRACT(EPOCH FROM (te.timestamp - tc.timestamp)) * 1000.0 \
                ELSE 0.0 END), 0.0)::DOUBLE PRECISION AS tool_ms \
        FROM brain_turns bt \
        LEFT JOIN tool_calls tc ON tc.session_id = bt.session_id \
            AND tc.sequence_num > bt.previous_response_sequence_num AND tc.sequence_num < bt.response_sequence_num \
        LEFT JOIN terminal_events tr ON tr.session_id = tc.session_id \
            AND tr.event_type = 'ToolResult' \
            AND tr.tool_id = (tc.payload -> 'data' ->> 'tool_id') \
        LEFT JOIN terminal_events te ON te.session_id = tc.session_id \
            AND te.event_type = 'ToolError' \
            AND te.tool_id = (tc.payload -> 'data' ->> 'tool_id') \
        GROUP BY bt.session_id, bt.turn_number \
    ) \
    SELECT s.tenant_id, s.storage_partition_id, s.contact_id, s.user_id, bt.session_id, bt.turn_number, \
        bt.finished_at, bt.response_data ->> 'model' AS model, \
        NULL::DOUBLE PRECISION AS pipeline_ms, \
        COALESCE((bt.response_data ->> 'duration_ms')::DOUBLE PRECISION, 0.0) AS llm_ms, \
        COALESCE(tm.tool_ms, 0.0) AS tool_ms, \
        COALESCE(tm.tool_call_count, 0)::BIGINT AS tool_call_count, \
        COALESCE((bt.response_data ->> 'input_tokens_uncached')::BIGINT, 0)::BIGINT AS input_tokens_uncached, \
        COALESCE((bt.response_data ->> 'input_tokens_cache_write')::BIGINT, 0)::BIGINT AS input_tokens_cache_write, \
        COALESCE((bt.response_data ->> 'input_tokens_cache_read')::BIGINT, 0)::BIGINT AS input_tokens_cache_read, \
        (COALESCE((bt.response_data ->> 'input_tokens_uncached')::BIGINT, 0) \
            + COALESCE((bt.response_data ->> 'input_tokens_cache_write')::BIGINT, 0) \
            + COALESCE((bt.response_data ->> 'input_tokens_cache_read')::BIGINT, 0))::BIGINT AS total_input_tokens, \
        COALESCE((bt.response_data ->> 'output_tokens')::BIGINT, 0)::BIGINT AS output_tokens, \
        COALESCE((bt.response_data ->> 'cost_cents')::BIGINT, 0)::BIGINT AS cost_cents, \
        $2::timestamptz AS export_version \
    FROM brain_turns bt \
    JOIN sessions s ON s.id = bt.session_id \
    LEFT JOIN tool_metrics tm ON tm.session_id = bt.session_id AND tm.turn_number = bt.turn_number";

/// `tool_call_fact` recompute: the `tool_call_analytics` logic filtered to `$1`,
/// enriched with tenant id and the enclosing `turn_number`, `$2` stamped as
/// `export_version`.
const TOOL_CALL_FACT_SQL: &str = "WITH input_sessions AS ( \
        SELECT DISTINCT input.session_id \
        FROM UNNEST($1::uuid[]) AS input(session_id) \
    ), \
    tool_calls AS ( \
        SELECT s.tenant_id, s.storage_partition_id, s.user_id, e.session_id, \
            e.sequence_num AS call_sequence_num, e.turn_number, e.timestamp AS called_at, \
            e.payload -> 'data' AS call_data \
        FROM input_sessions input \
        JOIN events e ON e.session_id = input.session_id \
        JOIN sessions s ON s.id = e.session_id \
        WHERE e.event_type = 'ToolCall' \
    ), \
    terminal_events AS ( \
        SELECT DISTINCT ON (e.session_id, e.event_type, e.payload -> 'data' ->> 'tool_id') \
            e.id, e.session_id, e.event_type, e.payload, e.timestamp, \
            e.payload -> 'data' ->> 'tool_id' AS tool_id \
        FROM input_sessions input \
        JOIN events e ON e.session_id = input.session_id \
        WHERE e.event_type IN ('ToolResult', 'ToolError') \
        ORDER BY e.session_id, e.event_type, e.payload -> 'data' ->> 'tool_id', e.sequence_num \
    ) \
    SELECT tc.tenant_id, tc.storage_partition_id, tc.user_id, tc.session_id, tc.call_sequence_num, \
        tc.turn_number, \
        (tc.call_data ->> 'tool_id')::UUID AS tool_id, \
        COALESCE(tc.call_data ->> 'tool_name', '') AS tool_name, \
        CASE \
            WHEN result_event.id IS NOT NULL THEN COALESCE((result_event.payload -> 'data' ->> 'success')::BOOLEAN, FALSE) \
            WHEN error_event.id IS NOT NULL THEN FALSE \
            ELSE FALSE END AS success, \
        CASE \
            WHEN result_event.id IS NOT NULL THEN COALESCE((result_event.payload -> 'data' ->> 'duration_ms')::DOUBLE PRECISION, EXTRACT(EPOCH FROM (result_event.timestamp - tc.called_at)) * 1000.0) \
            WHEN error_event.id IS NOT NULL THEN EXTRACT(EPOCH FROM (error_event.timestamp - tc.called_at)) * 1000.0 \
            ELSE NULL END AS duration_ms, \
        'main'::TEXT AS model_tier, tc.called_at AS ts, $2::timestamptz AS export_version \
    FROM tool_calls tc \
    LEFT JOIN terminal_events result_event ON result_event.session_id = tc.session_id \
        AND result_event.event_type = 'ToolResult' \
        AND result_event.tool_id = (tc.call_data ->> 'tool_id') \
    LEFT JOIN terminal_events error_event ON error_event.session_id = tc.session_id \
        AND error_event.event_type = 'ToolError' \
        AND error_event.tool_id = (tc.call_data ->> 'tool_id')";

impl AnalyticsExporter {
    /// Recomputes and upserts `turn_fact` / `tool_call_fact` for the given
    /// sessions, chunked to bound per-query fan-out. Every row of a pass shares
    /// one `export_version` so re-exports supersede prior copies on merge.
    pub async fn export_facts(&self, sessions: &[Uuid]) -> Result<(), ExportError> {
        if sessions.is_empty() {
            return Ok(());
        }
        let export_version = Utc::now();
        for chunk in sessions.chunks(FACT_SESSION_CHUNK) {
            let mut turn_tx = self.begin_read_txn().await?;
            let turn_rows: Vec<TurnFactRow> = sqlx::query_as::<_, TurnFactRow>(TURN_FACT_SQL)
                .bind(chunk)
                .bind(export_version)
                .fetch_all(&mut *turn_tx)
                .await?;
            turn_tx.commit().await?;
            self.insert_rows("turn_fact", &turn_rows).await?;
            record_rows("turn_fact", turn_rows.len() as u64);

            let mut tool_tx = self.begin_read_txn().await?;
            let tool_rows: Vec<ToolCallFactRow> =
                sqlx::query_as::<_, ToolCallFactRow>(TOOL_CALL_FACT_SQL)
                    .bind(chunk)
                    .bind(export_version)
                    .fetch_all(&mut *tool_tx)
                    .await?;
            tool_tx.commit().await?;
            self.insert_rows("tool_call_fact", &tool_rows).await?;
            record_rows("tool_call_fact", tool_rows.len() as u64);
        }
        Ok(())
    }
}

/// `turn_fact` row; field order matches the ClickHouse column order and the
/// `session_turn_metrics` projection.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct TurnFactRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub contact_id: Option<Uuid>,
    pub user_id: String,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    pub turn_number: i64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub finished_at: DateTime<Utc>,
    pub model: Option<String>,
    pub pipeline_ms: Option<f64>,
    pub llm_ms: f64,
    pub tool_ms: f64,
    pub tool_call_count: i64,
    pub input_tokens_uncached: i64,
    pub input_tokens_cache_write: i64,
    pub input_tokens_cache_read: i64,
    pub total_input_tokens: i64,
    pub output_tokens: i64,
    pub cost_cents: i64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `tool_call_fact` row; field order matches the ClickHouse column order.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct ToolCallFactRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub user_id: String,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    pub call_sequence_num: i64,
    pub turn_number: i64,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub tool_id: Option<Uuid>,
    pub tool_name: String,
    pub success: Option<bool>,
    pub duration_ms: Option<f64>,
    pub model_tier: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub ts: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}
