//! ClickHouse analytics exporter.
//!
//! When `[clickhouse]` is configured, this background loop incrementally pulls
//! changed rows from Postgres and lands derived analytics copies in ClickHouse:
//! the `events_raw` append stream, seven `dim_*` dimension tables, and the two
//! windowed fact tables (`turn_fact`, `tool_call_fact`) whose values are
//! computed in Postgres by reusing the `session_turn_metrics` /
//! `tool_call_analytics` SQL. Postgres stays the transactional source of truth;
//! ClickHouse holds analytical copies only.
//!
//! Only one pod exports at a time: leadership is a Postgres session advisory
//! lock (`clickhouse-analytics-export`) held on a dedicated connection for the
//! life of the loop; non-leaders sleep a poll interval and retry, and any pod
//! can take over when the leader's connection drops. Cursors persist in
//! `analytics.clickhouse_export_state` with a `2 × export_poll_secs` rewind
//! overlap; ClickHouse `ReplacingMergeTree` keys absorb the re-read rows.
//!
//! See `docs/plans/clickhouse-analytics-read-models.md` and
//! `docs/schemas/clickhouse-analytics.md` (the table contract).

mod dims;
mod events;
mod facts;
mod schema;

pub use dims::{
    DimArtifactNodeRunRow, DimArtifactRunRow, DimExperimentRunRow, DimLearningCandidateRow,
    DimSessionAgentContextRow, DimSessionRow, DimTaskSegmentRow,
};
pub use events::EventRawRow;
pub use facts::{ToolCallFactRow, TurnFactRow};

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clickhouse::{Client, Row};
use moa_core::config::ClickHouseConfig;
use serde::Serialize;
use sqlx::PgPool;
use sqlx::pool::PoolConnection;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Advisory-lock key string hashed for the single-writer export lease.
const EXPORT_LEASE_KEY: &str = "clickhouse-analytics-export";

/// Sessions processed per `export_facts` recompute query, bounding the
/// per-batch fan-out of the windowed fact SQL.
const FACT_SESSION_CHUNK: usize = 500;

/// `statement_timeout` (ms) applied to every OLTP pull and fact recompute so a
/// pathological poll can never camp on the source database.
const PULL_STATEMENT_TIMEOUT_MS: i32 = 30_000;

/// Errors raised while exporting analytics rows to ClickHouse.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// A Postgres read or cursor write failed.
    #[error("analytics export database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A ClickHouse DDL or insert failed.
    #[error("analytics export clickhouse error: {0}")]
    ClickHouse(#[from] clickhouse::error::Error),
}

/// Incremental Postgres-to-ClickHouse analytics exporter.
#[derive(Clone)]
pub struct AnalyticsExporter {
    pool: PgPool,
    clickhouse: Client,
    database: String,
    poll: Duration,
    overlap: Duration,
    batch_rows: i64,
}

impl std::fmt::Debug for AnalyticsExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyticsExporter")
            .field("database", &self.database)
            .field("poll", &self.poll)
            .field("overlap", &self.overlap)
            .field("batch_rows", &self.batch_rows)
            .finish_non_exhaustive()
    }
}

impl AnalyticsExporter {
    /// Builds an exporter from the validated `[clickhouse]` config section.
    #[must_use]
    pub fn from_config(pool: PgPool, config: &ClickHouseConfig) -> Self {
        let mut client = Client::default().with_url(config.url.trim());
        if let Some(user) = config.user.as_deref() {
            client = client.with_user(user);
        }
        if let Some(password) = config.password.as_deref() {
            client = client.with_password(password);
        }
        Self::with_client(
            pool,
            client,
            config.database.trim().to_string(),
            config.export_poll_secs,
            config.export_batch_rows,
        )
    }

    /// Builds an exporter from an explicit ClickHouse client (used by tests to
    /// point at a mock server).
    #[must_use]
    pub fn with_client(
        pool: PgPool,
        clickhouse: Client,
        database: String,
        poll_secs: u64,
        batch_rows: usize,
    ) -> Self {
        let poll = Duration::from_secs(poll_secs.max(1));
        Self {
            pool,
            clickhouse,
            database,
            poll,
            overlap: poll.saturating_mul(2),
            batch_rows: i64::try_from(batch_rows.max(1)).unwrap_or(i64::MAX),
        }
    }

    /// Runs the leader-leased export loop until the shutdown token fires.
    ///
    /// Non-leaders sleep one poll interval and retry the lease; the leader holds
    /// the advisory lock on `lease` for the life of the loop and releases it on
    /// shutdown so a peer can take over promptly.
    pub async fn run(self, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let lease = match self.acquire_lease().await {
                Ok(Some(lease)) => lease,
                Ok(None) => {
                    if self.sleep_or_cancel(&cancel).await {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "analytics export lease acquisition failed");
                    record_error();
                    if self.sleep_or_cancel(&cancel).await {
                        return;
                    }
                    continue;
                }
            };
            let mut lease = lease;
            if let Err(error) = self.ensure_clickhouse_schema().await {
                tracing::warn!(%error, "analytics export clickhouse schema bootstrap failed");
                record_error();
                self.release_lease(&mut lease).await;
                if self.sleep_or_cancel(&cancel).await {
                    return;
                }
                continue;
            }
            tracing::info!("analytics export: acquired leader lease");
            self.lead(&cancel).await;
            self.release_lease(&mut lease).await;
            return;
        }
    }

    /// Runs export passes on the poll cadence until cancelled (leader only).
    async fn lead(&self, cancel: &CancellationToken) {
        let mut interval = tokio::time::interval(self.poll);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = interval.tick() => {}
            }
            if let Err(error) = self.run_one_pass().await {
                tracing::warn!(%error, "analytics export pass failed");
                record_error();
            }
        }
    }

    /// Exports one full pass: dimensions, the events stream, then the windowed
    /// facts for the sessions touched by this pass's events batch.
    pub async fn run_one_pass(&self) -> Result<(), ExportError> {
        self.export_dim_sessions().await?;
        self.export_dim_session_agent_context().await?;
        self.export_dim_task_segments().await?;
        self.export_dim_artifact_run().await?;
        self.export_dim_artifact_node_run().await?;
        self.export_dim_learning_candidates().await?;
        self.export_dim_experiment_run().await?;
        let touched = self.export_events().await?;
        self.export_facts(&touched).await?;
        Ok(())
    }

    /// Tries to take the single-writer lease on a dedicated connection.
    async fn acquire_lease(&self) -> Result<Option<PoolConnection<sqlx::Postgres>>, ExportError> {
        let mut conn = self.pool.acquire().await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(EXPORT_LEASE_KEY)
            .fetch_one(&mut *conn)
            .await?;
        if acquired { Ok(Some(conn)) } else { Ok(None) }
    }

    /// Releases the advisory lock so the pooled connection does not carry the
    /// lease after this pod stops leading.
    async fn release_lease(&self, lease: &mut PoolConnection<sqlx::Postgres>) {
        if let Err(error) = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(EXPORT_LEASE_KEY)
            .execute(&mut **lease)
            .await
        {
            tracing::warn!(%error, "analytics export lease release failed");
        }
    }

    /// Sleeps one poll interval, returning `true` if cancelled meanwhile.
    async fn sleep_or_cancel(&self, cancel: &CancellationToken) -> bool {
        tokio::select! {
            _ = cancel.cancelled() => true,
            _ = tokio::time::sleep(self.poll) => false,
        }
    }

    /// Reads the persisted `(cursor_ts, cursor_id)` for one exported table.
    async fn read_cursor(
        &self,
        table: &str,
    ) -> Result<Option<(DateTime<Utc>, Option<Uuid>)>, ExportError> {
        let row: Option<(DateTime<Utc>, Option<Uuid>)> = sqlx::query_as(
            "SELECT cursor_ts, cursor_id FROM analytics.clickhouse_export_state \
             WHERE table_name = $1",
        )
        .bind(table)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Persists the cursor for one exported table after a successful insert.
    async fn write_cursor(
        &self,
        table: &str,
        cursor_ts: DateTime<Utc>,
        cursor_id: Option<Uuid>,
    ) -> Result<(), ExportError> {
        sqlx::query(
            "INSERT INTO analytics.clickhouse_export_state \
                 (table_name, cursor_ts, cursor_id, exported_at) \
             VALUES ($1, $2, $3, NOW()) \
             ON CONFLICT (table_name) DO UPDATE SET \
                 cursor_ts = EXCLUDED.cursor_ts, \
                 cursor_id = EXCLUDED.cursor_id, \
                 exported_at = NOW()",
        )
        .bind(table)
        .bind(cursor_ts)
        .bind(cursor_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns the overlap-rewound lower bound for a table's next pull.
    fn effective_lower_bound(
        &self,
        cursor: Option<(DateTime<Utc>, Option<Uuid>)>,
    ) -> DateTime<Utc> {
        effective_lower_bound(cursor.map(|(ts, _)| ts), self.overlap)
    }

    /// Begins a read transaction with a bounded `statement_timeout` so a single
    /// OLTP pull or fact recompute can never run unbounded on the source DB.
    async fn begin_read_txn(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, ExportError> {
        let mut tx = self.pool.begin().await?;
        // `SET LOCAL` cannot be parameterized; the value is a fixed constant.
        sqlx::query(&format!(
            "SET LOCAL statement_timeout = {PULL_STATEMENT_TIMEOUT_MS}"
        ))
        .execute(&mut *tx)
        .await?;
        Ok(tx)
    }

    /// Appends one batch of rows to a ClickHouse table; a no-op when empty.
    async fn insert_rows<T>(&self, table: &str, rows: &[T]) -> Result<(), ExportError>
    where
        T: Row + Serialize,
    {
        if rows.is_empty() {
            return Ok(());
        }
        let qualified = format!("{}.{table}", self.database);
        let mut insert = self.clickhouse.insert::<T>(&qualified)?;
        for row in rows {
            insert.write(row).await?;
        }
        insert.end().await?;
        Ok(())
    }
}

/// Spawns the analytics export loop when `[clickhouse]` is configured.
///
/// Returns `None` (and starts nothing) when ClickHouse analytics is disabled, so
/// the Postgres matview path stays the only backend.
#[must_use]
pub fn spawn_analytics_export(
    pool: PgPool,
    config: Option<&ClickHouseConfig>,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let config = config?;
    let exporter = AnalyticsExporter::from_config(pool, config);
    tracing::info!(
        poll_secs = config.export_poll_secs,
        batch_rows = config.export_batch_rows,
        "starting clickhouse analytics exporter"
    );
    Some(tokio::spawn(exporter.run(cancel)))
}

/// Pure overlap math: rewind `cursor` by `overlap`, or start from the epoch when
/// no cursor exists yet (a zero-cursor backfill).
fn effective_lower_bound(cursor: Option<DateTime<Utc>>, overlap: Duration) -> DateTime<Utc> {
    match cursor {
        Some(cursor_ts) => {
            let overlap =
                chrono::Duration::from_std(overlap).unwrap_or_else(|_| chrono::Duration::zero());
            cursor_ts - overlap
        }
        None => DateTime::<Utc>::UNIX_EPOCH,
    }
}

/// Records the number of rows exported to a table.
fn record_rows(table: &'static str, rows: u64) {
    metrics::counter!("moa_analytics_export_rows_total", "table" => table).increment(rows);
}

/// Records export freshness lag: seconds between now and the exported-through ts.
fn record_lag(table: &'static str, cursor_ts: DateTime<Utc>) {
    let lag = (Utc::now() - cursor_ts).num_milliseconds() as f64 / 1000.0;
    metrics::gauge!("moa_analytics_export_lag_seconds", "table" => table).set(lag.max(0.0));
}

/// Records one export pass or lease failure.
fn record_error() {
    metrics::counter!("moa_analytics_export_errors_total").increment(1);
}

/// Records a source row skipped because its TEXT tenant id did not parse as UUID.
fn record_tenant_skip(table: &'static str) {
    metrics::counter!("moa_analytics_export_skipped_rows_total", "table" => table).increment(1);
}

/// Collects the distinct session ids from an events batch, preserving nothing
/// about order (facts recompute is set-based).
fn distinct_sessions(sessions: HashSet<Uuid>) -> Vec<Uuid> {
    sessions.into_iter().collect()
}

/// Parses a TEXT tenant id into a UUID, returning `None` for unparseable values
/// so the caller can skip the row.
pub(crate) fn parse_tenant_uuid(raw: &str) -> Option<Uuid> {
    Uuid::parse_str(raw.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_lower_bound_rewinds_by_overlap() {
        // Pins: an existing cursor rewinds by exactly the overlap window so the
        // next pull re-reads the boundary rows ReplacingMergeTree will dedup.
        let cursor = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(1_000);
        let overlap = Duration::from_secs(30);

        let bound = effective_lower_bound(Some(cursor), overlap);

        assert_eq!(bound, cursor - chrono::Duration::seconds(30));
    }

    #[test]
    fn effective_lower_bound_starts_at_epoch_without_cursor() {
        // Pins: a missing cursor backfills from the epoch (zero cursor) rather
        // than skipping history.
        let bound = effective_lower_bound(None, Duration::from_secs(30));

        assert_eq!(bound, DateTime::<Utc>::UNIX_EPOCH);
    }

    #[test]
    fn parse_tenant_uuid_skips_unparseable_text() {
        // Pins: TEXT tenant ids that are not UUIDs are dropped (skipped) rather
        // than aborting the whole dim batch.
        assert!(parse_tenant_uuid("not-a-uuid").is_none());
        let valid = Uuid::now_v7();
        assert_eq!(parse_tenant_uuid(&valid.to_string()), Some(valid));
    }
}
