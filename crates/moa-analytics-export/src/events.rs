//! Events stream export: incremental `(timestamp, id)`-cursored pull of `events`
//! into the `events_raw` append stream.
//!
//! Each exported event is stamped with `turn_number` — a stable pure function of
//! the session prefix, `1 + count of BrainResponse events with lower
//! sequence_num in the same session` — so ClickHouse-side aggregation never needs
//! cross-row context. A BrainResponse counts itself (its `turn_number` is its
//! ROW_NUMBER among the session's BrainResponses). The overlap window re-reads
//! boundary rows; `events_raw`'s `ReplacingMergeTree` key absorbs them.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::{AnalyticsExporter, ExportError, distinct_sessions, record_lag, record_rows};

/// SQL for one `events_raw` batch. `$1`/`$2` are the `(timestamp, id)` lower
/// bound (`$2` NULL on the first batch of a pass), `$3` the batch size. The
/// correlated `turn_number` count runs against the full session history via the
/// `(session_id, event_type)` index, so it is correct regardless of where the
/// cursor slices a session.
const EVENTS_SQL: &str = "SELECT e.id AS event_id, e.session_id, s.tenant_id, \
        e.storage_partition_id, e.user_id, e.sequence_num, \
        (1 + (SELECT COUNT(*) FROM events b \
              WHERE b.session_id = e.session_id AND b.event_type = 'BrainResponse' \
                AND b.sequence_num < e.sequence_num))::BIGINT AS turn_number, \
        e.event_type, e.token_count, e.payload::text AS payload, e.timestamp AS ts \
     FROM events e \
     JOIN sessions s ON s.id = e.session_id \
     WHERE (e.timestamp > $1 OR (e.timestamp = $1 AND ($2::uuid IS NULL OR e.id > $2))) \
     ORDER BY e.timestamp, e.id LIMIT $3";

impl AnalyticsExporter {
    /// Exports new `events` rows into `events_raw`, returning the distinct
    /// session ids the batch touched (for the windowed fact recompute).
    pub async fn export_events(&self) -> Result<Vec<Uuid>, ExportError> {
        const TABLE: &str = "events_raw";
        let cursor = self.read_cursor(TABLE).await?;
        let effective = self.effective_lower_bound(cursor);
        let mut after: Option<(DateTime<Utc>, Uuid)> = None;
        let mut touched: HashSet<Uuid> = HashSet::new();
        let mut last_cursor_ts: Option<DateTime<Utc>> = None;

        loop {
            let (bound_ts, bound_id) = match after {
                Some((ts, id)) => (ts, Some(id)),
                None => (effective, None),
            };
            let mut tx = self.begin_read_txn().await?;
            let rows: Vec<EventRawRow> = sqlx::query_as::<_, EventRawRow>(EVENTS_SQL)
                .bind(bound_ts)
                .bind(bound_id)
                .bind(self.batch_rows)
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            let Some(last_row) = rows.last() else {
                break;
            };
            let batch_len = rows.len();
            let last = (last_row.ts, last_row.event_id);
            for row in &rows {
                touched.insert(row.session_id);
            }

            self.insert_rows(TABLE, &rows).await?;
            record_rows(TABLE, rows.len() as u64);
            self.write_cursor(TABLE, last.0, Some(last.1)).await?;
            last_cursor_ts = Some(last.0);
            after = Some(last);

            if batch_len < self.batch_rows as usize {
                break;
            }
        }

        if let Some(cursor_ts) = last_cursor_ts {
            record_lag(TABLE, cursor_ts);
        }
        Ok(distinct_sessions(touched))
    }
}

/// `events_raw` row; field order matches the ClickHouse column order.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct EventRawRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub event_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub user_id: String,
    pub sequence_num: i64,
    pub turn_number: i64,
    pub event_type: String,
    pub token_count: Option<i32>,
    pub payload: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub ts: DateTime<Utc>,
}
