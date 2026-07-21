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
    DimExecutionRunRow, DimExecutionTaskRow, DimExperimentRunRow, DimLearningCandidateRow,
    DimSessionAgentContextRow, DimSessionRow, DimTaskSegmentRow,
};
pub use events::EventRawRow;
pub use facts::{ToolCallFactRow, TurnFactRow};

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use clickhouse::{Client, Row};
use moa_config::ClickHouseConfig;
use serde::{Deserialize, Serialize};
use sqlx::pool::PoolConnection;
use sqlx::{FromRow, PgPool};
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

/// Task 11's transaction advisory-lock namespace and key.
const EXECUTION_ANALYTICS_LOCK_SQL: &str = "SELECT pg_advisory_xact_lock(1297047877, 337)";

/// Durable key for the execution dimension schema/backfill state machine.
const EXECUTION_UPGRADE_KEY: &str = "execution_dimensions";

const EXECUTION_RUN_TABLE: &str = "dim_execution_runs";
const EXECUTION_TASK_TABLE: &str = "dim_execution_tasks";

/// Errors raised while exporting analytics rows to ClickHouse.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// A Postgres read or cursor write failed.
    #[error("analytics export database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A ClickHouse DDL or insert failed.
    #[error("analytics export clickhouse error: {0}")]
    ClickHouse(#[from] clickhouse::error::Error),
    /// The Postgres or ClickHouse execution analytics contract is inconsistent.
    #[error("analytics export contract error: {0}")]
    Contract(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecutionPosition {
    seq: i64,
    id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExecutionClickHouseIdentities {
    database_uuid: Uuid,
    run_table_uuid: Uuid,
    task_table_uuid: Uuid,
}

impl ExecutionPosition {
    const ZERO: Self = Self {
        seq: 0,
        id: Uuid::nil(),
    };

    fn max(self, other: Self) -> Self {
        if (other.seq, other.id) > (self.seq, self.id) {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, FromRow)]
struct ExecutionUpgradeState {
    generation: i64,
    database_uuid: Uuid,
    run_table_uuid: Uuid,
    task_table_uuid: Uuid,
    stage: String,
    upgrade_version: DateTime<Utc>,
    run_high_water_seq: i64,
    run_high_water_id: Uuid,
    task_high_water_seq: i64,
    task_high_water_id: Uuid,
    run_page_seq: i64,
    run_page_id: Uuid,
    task_page_seq: i64,
    task_page_id: Uuid,
}

impl ExecutionUpgradeState {
    fn clickhouse_identities(&self) -> ExecutionClickHouseIdentities {
        ExecutionClickHouseIdentities {
            database_uuid: self.database_uuid,
            run_table_uuid: self.run_table_uuid,
            task_table_uuid: self.task_table_uuid,
        }
    }

    fn run_high_water(&self) -> ExecutionPosition {
        ExecutionPosition {
            seq: self.run_high_water_seq,
            id: self.run_high_water_id,
        }
    }

    fn task_high_water(&self) -> ExecutionPosition {
        ExecutionPosition {
            seq: self.task_high_water_seq,
            id: self.task_high_water_id,
        }
    }

    fn run_page(&self) -> ExecutionPosition {
        ExecutionPosition {
            seq: self.run_page_seq,
            id: self.run_page_id,
        }
    }

    fn task_page(&self) -> ExecutionPosition {
        ExecutionPosition {
            seq: self.task_page_seq,
            id: self.task_page_id,
        }
    }
}

#[derive(Debug, FromRow)]
struct ExecutionUpgradeHead {
    generation: i64,
    database_uuid: Uuid,
    run_table_uuid: Uuid,
    task_table_uuid: Uuid,
    export_version_floor: DateTime<Utc>,
}

impl ExecutionUpgradeHead {
    #[cfg(test)]
    fn clickhouse_identities(&self) -> ExecutionClickHouseIdentities {
        ExecutionClickHouseIdentities {
            database_uuid: self.database_uuid,
            run_table_uuid: self.run_table_uuid,
            task_table_uuid: self.task_table_uuid,
        }
    }
}

#[derive(Debug, FromRow)]
struct ExecutionCursorState {
    cursor_seq: i64,
    cursor_id: Uuid,
    pass_high_water_seq: Option<i64>,
    pass_high_water_id: Option<Uuid>,
    pass_started_at: Option<DateTime<Utc>>,
}

impl ExecutionCursorState {
    fn cursor(&self) -> ExecutionPosition {
        ExecutionPosition {
            seq: self.cursor_seq,
            id: self.cursor_id,
        }
    }

    fn high_water(&self) -> Result<Option<ExecutionPosition>, ExportError> {
        match (
            self.pass_high_water_seq,
            self.pass_high_water_id,
            self.pass_started_at,
        ) {
            (None, None, None) => Ok(None),
            (Some(seq), Some(id), Some(_)) => Ok(Some(ExecutionPosition { seq, id })),
            _ => Err(ExportError::Contract(
                "execution cursor active-pass tuple is partially null".to_string(),
            )),
        }
    }
}

#[derive(Debug, Row, Deserialize)]
struct MaxExecutionVersionRow {
    export_version_micros: Option<i64>,
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
        self.export_execution_dimensions().await?;
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

    pub(crate) async fn ensure_execution_dimension_upgrade(
        &self,
        identities: ExecutionClickHouseIdentities,
    ) -> Result<(), ExportError> {
        if self
            .read_execution_upgrade_state()
            .await?
            .is_none_or(|state| state.clickhouse_identities() != identities)
        {
            self.initialize_execution_upgrade(identities).await?;
        }

        loop {
            let state = self.read_execution_upgrade_state().await?.ok_or_else(|| {
                ExportError::Contract(
                    "execution dimension upgrade state disappeared after initialization"
                        .to_string(),
                )
            })?;
            if state.clickhouse_identities() != identities {
                return Err(ExportError::Contract(format!(
                    "execution bootstrap generation {} is bound to ClickHouse database/table \
                     UUIDs ({}, {}, {}), but startup validated ({}, {}, {})",
                    state.generation,
                    state.database_uuid,
                    state.run_table_uuid,
                    state.task_table_uuid,
                    identities.database_uuid,
                    identities.run_table_uuid,
                    identities.task_table_uuid
                )));
            }
            match state.stage.as_str() {
                "pending" => {
                    self.advance_upgrade_stage(state.generation, "pending", "schema_upgraded")
                        .await?
                }
                "schema_upgraded" => self.reset_execution_upgrade_cursors(&state).await?,
                "cursors_reset" => self.export_execution_upgrade_runs(&state).await?,
                "runs_exported" => self.export_execution_upgrade_tasks(&state).await?,
                "tasks_exported" => self.complete_execution_upgrade(&state).await?,
                "complete" => return Ok(()),
                stage => {
                    return Err(ExportError::Contract(format!(
                        "unknown execution dimension upgrade stage {stage}"
                    )));
                }
            }
        }
    }

    async fn read_execution_upgrade_state(
        &self,
    ) -> Result<Option<ExecutionUpgradeState>, ExportError> {
        Ok(sqlx::query_as::<_, ExecutionUpgradeState>(
            "SELECT generation, database_uuid, run_table_uuid, task_table_uuid, stage, \
                    upgrade_version, \
                    run_high_water_seq, run_high_water_id, \
                    task_high_water_seq, task_high_water_id, run_page_seq, run_page_id, \
                    task_page_seq, task_page_id \
             FROM analytics.clickhouse_schema_upgrade_state WHERE upgrade_key = $1 \
             ORDER BY generation DESC LIMIT 1",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn initialize_execution_upgrade(
        &self,
        identities: ExecutionClickHouseIdentities,
    ) -> Result<(), ExportError> {
        let max_existing = self.max_existing_execution_export_version().await?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(EXECUTION_ANALYTICS_LOCK_SQL)
            .execute(&mut *tx)
            .await?;
        let previous = sqlx::query_as::<_, ExecutionUpgradeHead>(
            "SELECT generation, database_uuid, run_table_uuid, task_table_uuid, \
                    export_version_floor \
             FROM analytics.clickhouse_schema_upgrade_state \
             WHERE upgrade_key = $1 ORDER BY generation DESC LIMIT 1 FOR UPDATE",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous.as_ref()
            && !execution_reset_starts_new_generation(previous, identities, &self.database)?
        {
            tx.commit().await?;
            return Ok(());
        }
        let run_high_water = Self::greatest_execution_position(
            &mut tx,
            "SELECT analytics_change_seq, run_uid \
             FROM moa.execution_run \
             ORDER BY analytics_change_seq DESC, run_uid DESC LIMIT 1",
        )
        .await?;
        let task_high_water = Self::greatest_execution_position(
            &mut tx,
            "SELECT analytics_change_seq, task_id \
             FROM moa.execution_task \
             ORDER BY analytics_change_seq DESC, task_id DESC LIMIT 1",
        )
        .await?;
        let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let prior_floor = previous.as_ref().map(|state| state.export_version_floor);
        let version_floor = [max_existing, prior_floor].into_iter().flatten().max();
        let upgrade_version = monotonic_export_version(database_now, version_floor)?;
        let generation = match previous {
            Some(previous) => previous.generation.checked_add(1).ok_or_else(|| {
                ExportError::Contract(
                    "execution bootstrap generation cannot advance beyond i64::MAX".to_string(),
                )
            })?,
            None => 1,
        };
        sqlx::query(
            "INSERT INTO analytics.clickhouse_schema_upgrade_state (
                 upgrade_key, generation, database_uuid, run_table_uuid, task_table_uuid,
                 stage, upgrade_version, export_version_floor,
                 run_high_water_seq, run_high_water_id,
                 task_high_water_seq, task_high_water_id,
                 run_page_seq, run_page_id, task_page_seq, task_page_id
             ) VALUES ($1, $2, $3, $4, $5, 'pending', $6, $6, $7, $8, $9, $10, 0, $11, 0, $11)",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .bind(generation)
        .bind(identities.database_uuid)
        .bind(identities.run_table_uuid)
        .bind(identities.task_table_uuid)
        .bind(upgrade_version)
        .bind(run_high_water.seq)
        .bind(run_high_water.id)
        .bind(task_high_water.seq)
        .bind(task_high_water.id)
        .bind(Uuid::nil())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn greatest_execution_position(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        sql: &str,
    ) -> Result<ExecutionPosition, ExportError> {
        let row: Option<(i64, Uuid)> = sqlx::query_as(sql).fetch_optional(&mut **tx).await?;
        Ok(row
            .map(|(seq, id)| ExecutionPosition { seq, id })
            .unwrap_or(ExecutionPosition::ZERO))
    }

    async fn max_existing_execution_export_version(
        &self,
    ) -> Result<Option<DateTime<Utc>>, ExportError> {
        let row = self
            .clickhouse
            .query(
                "SELECT toUnixTimestamp64Micro(maxOrNull(export_version)) \
                     AS export_version_micros \
                 FROM (
                     SELECT export_version FROM ?.dim_execution_runs
                     UNION ALL
                     SELECT export_version FROM ?.dim_execution_tasks
                 )",
            )
            .bind(clickhouse::sql::Identifier(&self.database))
            .bind(clickhouse::sql::Identifier(&self.database))
            .fetch_one::<MaxExecutionVersionRow>()
            .await?;
        row.export_version_micros
            .map(|micros| {
                DateTime::<Utc>::from_timestamp_micros(micros).ok_or_else(|| {
                    ExportError::Contract(format!(
                        "ClickHouse execution export_version {micros}us is out of range"
                    ))
                })
            })
            .transpose()
    }

    async fn advance_upgrade_stage(
        &self,
        generation: i64,
        expected: &str,
        next: &str,
    ) -> Result<(), ExportError> {
        let result = sqlx::query(
            "UPDATE analytics.clickhouse_schema_upgrade_state \
             SET stage = $4, updated_at = NOW() \
             WHERE upgrade_key = $1 AND generation = $2 AND stage = $3",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .bind(generation)
        .bind(expected)
        .bind(next)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution upgrade stage transition {expected}->{next} affected {} rows",
                result.rows_affected()
            )));
        }
        Ok(())
    }

    async fn reset_execution_upgrade_cursors(
        &self,
        state: &ExecutionUpgradeState,
    ) -> Result<(), ExportError> {
        let mut tx = self.pool.begin().await?;
        for table in [EXECUTION_RUN_TABLE, EXECUTION_TASK_TABLE] {
            sqlx::query(
                "INSERT INTO analytics.clickhouse_export_state (
                     table_name, cursor_ts, cursor_id, exported_at, cursor_seq,
                     pass_high_water_seq, pass_high_water_id, pass_started_at
                 ) VALUES ($1, $2, $3, $2, 0, NULL, NULL, NULL)
                 ON CONFLICT (table_name) DO UPDATE SET
                     cursor_ts = EXCLUDED.cursor_ts,
                     cursor_id = EXCLUDED.cursor_id,
                     exported_at = EXCLUDED.exported_at,
                     cursor_seq = EXCLUDED.cursor_seq,
                     pass_high_water_seq = NULL,
                     pass_high_water_id = NULL,
                     pass_started_at = NULL",
            )
            .bind(table)
            .bind(DateTime::<Utc>::UNIX_EPOCH)
            .bind(Uuid::nil())
            .execute(&mut *tx)
            .await?;
        }
        let result = sqlx::query(
            "UPDATE analytics.clickhouse_schema_upgrade_state \
             SET stage = 'cursors_reset', run_page_seq = 0, run_page_id = $3, \
                 task_page_seq = 0, task_page_id = $3, updated_at = NOW() \
             WHERE upgrade_key = $1 AND generation = $2 AND stage = 'schema_upgraded'",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .bind(state.generation)
        .bind(Uuid::nil())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution cursor reset affected {} upgrade rows",
                result.rows_affected()
            )));
        }
        tx.commit().await?;
        Ok(())
    }

    async fn export_execution_upgrade_runs(
        &self,
        state: &ExecutionUpgradeState,
    ) -> Result<(), ExportError> {
        let mut page = state.run_page();
        let high_water = state.run_high_water();
        loop {
            let rows = self.read_execution_run_page(page, high_water).await?;
            let Some(last) = rows.last() else {
                if page != high_water {
                    self.write_upgrade_page(
                        state.generation,
                        "run_page_seq",
                        "run_page_id",
                        high_water,
                    )
                    .await?;
                }
                self.advance_upgrade_stage(state.generation, "cursors_reset", "runs_exported")
                    .await?;
                return Ok(());
            };
            let last = ExecutionPosition {
                seq: last.analytics_change_seq,
                id: last.run_uid,
            };
            let clickhouse_rows = rows
                .into_iter()
                .map(|row| row.into_clickhouse(state.upgrade_version))
                .collect::<Result<Vec<_>, _>>()?;
            self.insert_rows(EXECUTION_RUN_TABLE, &clickhouse_rows)
                .await?;
            record_rows(EXECUTION_RUN_TABLE, clickhouse_rows.len() as u64);
            self.write_upgrade_page(state.generation, "run_page_seq", "run_page_id", last)
                .await?;
            page = last;
        }
    }

    async fn export_execution_upgrade_tasks(
        &self,
        state: &ExecutionUpgradeState,
    ) -> Result<(), ExportError> {
        let mut page = state.task_page();
        let high_water = state.task_high_water();
        loop {
            let rows = self.read_execution_task_page(page, high_water).await?;
            let Some(last) = rows.last() else {
                if page != high_water {
                    self.write_upgrade_page(
                        state.generation,
                        "task_page_seq",
                        "task_page_id",
                        high_water,
                    )
                    .await?;
                }
                self.advance_upgrade_stage(state.generation, "runs_exported", "tasks_exported")
                    .await?;
                return Ok(());
            };
            let last = ExecutionPosition {
                seq: last.analytics_change_seq,
                id: last.task_id,
            };
            let clickhouse_rows = rows
                .into_iter()
                .map(|row| row.into_clickhouse(state.upgrade_version))
                .collect::<Result<Vec<_>, _>>()?;
            self.insert_rows(EXECUTION_TASK_TABLE, &clickhouse_rows)
                .await?;
            record_rows(EXECUTION_TASK_TABLE, clickhouse_rows.len() as u64);
            self.write_upgrade_page(state.generation, "task_page_seq", "task_page_id", last)
                .await?;
            page = last;
        }
    }

    async fn write_upgrade_page(
        &self,
        generation: i64,
        seq_column: &str,
        id_column: &str,
        position: ExecutionPosition,
    ) -> Result<(), ExportError> {
        let sql = format!(
            "UPDATE analytics.clickhouse_schema_upgrade_state \
             SET {seq_column} = $3, {id_column} = $4, updated_at = NOW() \
             WHERE upgrade_key = $1 AND generation = $2"
        );
        let result = sqlx::query(&sql)
            .bind(EXECUTION_UPGRADE_KEY)
            .bind(generation)
            .bind(position.seq)
            .bind(position.id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution bootstrap generation {generation} page update affected {} rows",
                result.rows_affected()
            )));
        }
        Ok(())
    }

    async fn complete_execution_upgrade(
        &self,
        state: &ExecutionUpgradeState,
    ) -> Result<(), ExportError> {
        let mut tx = self.pool.begin().await?;
        let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        for (table, high_water) in [
            (EXECUTION_RUN_TABLE, state.run_high_water()),
            (EXECUTION_TASK_TABLE, state.task_high_water()),
        ] {
            sqlx::query(
                "UPDATE analytics.clickhouse_export_state \
                 SET cursor_seq = $2, cursor_id = $3, cursor_ts = $4, exported_at = $4, \
                     pass_high_water_seq = NULL, pass_high_water_id = NULL, pass_started_at = NULL \
                 WHERE table_name = $1",
            )
            .bind(table)
            .bind(high_water.seq)
            .bind(high_water.id)
            .bind(database_now)
            .execute(&mut *tx)
            .await?;
        }
        let result = sqlx::query(
            "UPDATE analytics.clickhouse_schema_upgrade_state \
             SET stage = 'complete', completed_at = $3, updated_at = $3 \
             WHERE upgrade_key = $1 AND generation = $2 AND stage = 'tasks_exported'",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .bind(state.generation)
        .bind(database_now)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution upgrade completion affected {} rows",
                result.rows_affected()
            )));
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn prepare_execution_passes(&self) -> Result<(), ExportError> {
        let state = self.read_execution_upgrade_state().await?;
        if state.as_ref().map(|state| state.stage.as_str()) != Some("complete") {
            return Err(ExportError::Contract(
                "normal execution export is paused until execution_dimensions is complete"
                    .to_string(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(EXECUTION_ANALYTICS_LOCK_SQL)
            .execute(&mut *tx)
            .await?;
        let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        self.prepare_execution_pass(
            &mut tx,
            EXECUTION_RUN_TABLE,
            "SELECT analytics_change_seq, run_uid FROM moa.execution_run \
             ORDER BY analytics_change_seq DESC, run_uid DESC LIMIT 1",
            database_now,
        )
        .await?;
        self.prepare_execution_pass(
            &mut tx,
            EXECUTION_TASK_TABLE,
            "SELECT analytics_change_seq, task_id FROM moa.execution_task \
             ORDER BY analytics_change_seq DESC, task_id DESC LIMIT 1",
            database_now,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn prepare_execution_pass(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        table: &str,
        high_water_sql: &str,
        database_now: DateTime<Utc>,
    ) -> Result<(), ExportError> {
        let row: ExecutionCursorState = sqlx::query_as(
            "SELECT cursor_seq, cursor_id, pass_high_water_seq, pass_high_water_id, \
                    pass_started_at \
             FROM analytics.clickhouse_export_state \
             WHERE table_name = $1 FOR UPDATE",
        )
        .bind(table)
        .fetch_one(&mut **tx)
        .await?;
        if row.high_water()?.is_some() {
            return Ok(());
        }
        let source_high_water = Self::greatest_execution_position(tx, high_water_sql).await?;
        let high_water = row.cursor().max(source_high_water);
        sqlx::query(
            "UPDATE analytics.clickhouse_export_state \
             SET pass_high_water_seq = $2, pass_high_water_id = $3, pass_started_at = $4 \
             WHERE table_name = $1",
        )
        .bind(table)
        .bind(high_water.seq)
        .bind(high_water.id)
        .bind(database_now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub(crate) async fn export_execution_run_pass(&self) -> Result<(), ExportError> {
        loop {
            let state = self.read_execution_cursor(EXECUTION_RUN_TABLE).await?;
            let high_water = state.high_water()?.ok_or_else(|| {
                ExportError::Contract("execution run pass has no active high water".to_string())
            })?;
            let cursor = state.cursor();
            let rows = self.read_execution_run_page(cursor, high_water).await?;
            let Some(last) = rows.last() else {
                self.complete_execution_pass(EXECUTION_RUN_TABLE, high_water)
                    .await?;
                return Ok(());
            };
            let last = ExecutionPosition {
                seq: last.analytics_change_seq,
                id: last.run_uid,
            };
            let export_version = self.claim_execution_export_version().await?;
            let clickhouse_rows = rows
                .into_iter()
                .map(|row| row.into_clickhouse(export_version))
                .collect::<Result<Vec<_>, _>>()?;
            self.insert_rows(EXECUTION_RUN_TABLE, &clickhouse_rows)
                .await?;
            record_rows(EXECUTION_RUN_TABLE, clickhouse_rows.len() as u64);
            self.advance_execution_cursor(EXECUTION_RUN_TABLE, last)
                .await?;
            if last == high_water {
                self.complete_execution_pass(EXECUTION_RUN_TABLE, high_water)
                    .await?;
                return Ok(());
            }
        }
    }

    pub(crate) async fn export_execution_task_pass(&self) -> Result<(), ExportError> {
        loop {
            let state = self.read_execution_cursor(EXECUTION_TASK_TABLE).await?;
            let high_water = state.high_water()?.ok_or_else(|| {
                ExportError::Contract("execution task pass has no active high water".to_string())
            })?;
            let cursor = state.cursor();
            let rows = self.read_execution_task_page(cursor, high_water).await?;
            let Some(last) = rows.last() else {
                self.complete_execution_pass(EXECUTION_TASK_TABLE, high_water)
                    .await?;
                return Ok(());
            };
            let last = ExecutionPosition {
                seq: last.analytics_change_seq,
                id: last.task_id,
            };
            let export_version = self.claim_execution_export_version().await?;
            let clickhouse_rows = rows
                .into_iter()
                .map(|row| row.into_clickhouse(export_version))
                .collect::<Result<Vec<_>, _>>()?;
            self.insert_rows(EXECUTION_TASK_TABLE, &clickhouse_rows)
                .await?;
            record_rows(EXECUTION_TASK_TABLE, clickhouse_rows.len() as u64);
            self.advance_execution_cursor(EXECUTION_TASK_TABLE, last)
                .await?;
            if last == high_water {
                self.complete_execution_pass(EXECUTION_TASK_TABLE, high_water)
                    .await?;
                return Ok(());
            }
        }
    }

    async fn read_execution_cursor(
        &self,
        table: &str,
    ) -> Result<ExecutionCursorState, ExportError> {
        Ok(sqlx::query_as(
            "SELECT cursor_seq, cursor_id, pass_high_water_seq, pass_high_water_id, \
                    pass_started_at \
             FROM analytics.clickhouse_export_state WHERE table_name = $1",
        )
        .bind(table)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn claim_execution_export_version(&self) -> Result<DateTime<Utc>, ExportError> {
        let mut tx = self.pool.begin().await?;
        let (generation, floor): (i64, DateTime<Utc>) = sqlx::query_as(
            "SELECT generation, export_version_floor \
             FROM analytics.clickhouse_schema_upgrade_state \
             WHERE upgrade_key = $1 ORDER BY generation DESC LIMIT 1 FOR UPDATE",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .fetch_one(&mut *tx)
        .await?;
        let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let next = monotonic_export_version(database_now, Some(floor))?;
        let result = sqlx::query(
            "UPDATE analytics.clickhouse_schema_upgrade_state \
             SET export_version_floor = $3, updated_at = NOW() \
             WHERE upgrade_key = $1 AND generation = $2",
        )
        .bind(EXECUTION_UPGRADE_KEY)
        .bind(generation)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution bootstrap generation {generation} version claim affected {} rows",
                result.rows_affected()
            )));
        }
        tx.commit().await?;
        Ok(next)
    }

    async fn advance_execution_cursor(
        &self,
        table: &str,
        position: ExecutionPosition,
    ) -> Result<(), ExportError> {
        sqlx::query(
            "UPDATE analytics.clickhouse_export_state \
             SET cursor_seq = $2, cursor_id = $3 \
             WHERE table_name = $1 \
               AND (cursor_seq, cursor_id) < ($2, $3)",
        )
        .bind(table)
        .bind(position.seq)
        .bind(position.id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn complete_execution_pass(
        &self,
        table: &'static str,
        high_water: ExecutionPosition,
    ) -> Result<(), ExportError> {
        let mut tx = self.pool.begin().await?;
        let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT transaction_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let result = sqlx::query(
            "UPDATE analytics.clickhouse_export_state \
             SET cursor_seq = $2, cursor_id = $3, \
                 pass_high_water_seq = NULL, pass_high_water_id = NULL, pass_started_at = NULL, \
                 cursor_ts = $4, exported_at = $4 \
             WHERE table_name = $1 \
               AND pass_high_water_seq = $2 AND pass_high_water_id = $3",
        )
        .bind(table)
        .bind(high_water.seq)
        .bind(high_water.id)
        .bind(database_now)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ExportError::Contract(format!(
                "execution pass completion for {table} affected {} rows",
                result.rows_affected()
            )));
        }
        tx.commit().await?;
        record_lag(table, database_now);
        Ok(())
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

fn monotonic_export_version(
    database_now: DateTime<Utc>,
    floor: Option<DateTime<Utc>>,
) -> Result<DateTime<Utc>, ExportError> {
    let Some(floor) = floor else {
        return Ok(database_now);
    };
    let after_floor = floor
        .checked_add_signed(chrono::Duration::microseconds(1))
        .ok_or_else(|| {
            ExportError::Contract(format!(
                "execution export version floor {floor} cannot advance by one microsecond"
            ))
        })?;
    Ok(database_now.max(after_floor))
}

fn execution_reset_starts_new_generation(
    previous: &ExecutionUpgradeHead,
    current: ExecutionClickHouseIdentities,
    database: &str,
) -> Result<bool, ExportError> {
    if current.database_uuid != previous.database_uuid {
        return Err(ExportError::Contract(format!(
            "unsafe ClickHouse analytics database reset detected for {database}: database UUID \
             changed from {} to {}; persisted cursors for non-execution tables cannot be \
             replayed safely. Do not drop the database; reset execution analytics by dropping \
             exactly dim_execution_runs and dim_execution_tasks together inside the existing \
             database",
            previous.database_uuid, current.database_uuid
        )));
    }

    let run_changed = current.run_table_uuid != previous.run_table_uuid;
    let task_changed = current.task_table_uuid != previous.task_table_uuid;
    match (run_changed, task_changed) {
        (false, false) => Ok(false),
        (true, true) => Ok(true),
        _ => Err(ExportError::Contract(format!(
            "unsafe partial execution analytics reset detected in {database}: execution table \
             UUIDs must change together (dim_execution_runs {} -> {}, dim_execution_tasks {} \
             -> {}). Drop exactly both execution tables inside the existing ClickHouse database \
             and restart",
            previous.run_table_uuid,
            current.run_table_uuid,
            previous.task_table_uuid,
            current.task_table_uuid
        ))),
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

    #[test]
    fn execution_export_versions_advance_under_clock_skew() {
        // Pins: a database clock behind the prior ClickHouse/export-state
        // version still claims a strictly newer microsecond version.
        let floor = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::seconds(10);
        let skewed_now = floor - chrono::Duration::seconds(5);

        let claimed = monotonic_export_version(skewed_now, Some(floor))
            .expect("representable floor must advance");

        assert_eq!(claimed, floor + chrono::Duration::microseconds(1));
    }

    #[test]
    fn execution_positions_compare_sequence_before_uuid() {
        // Pins: sequence order is primary and UUID only breaks ties.
        let low_uuid = Uuid::from_u128(1);
        let high_uuid = Uuid::from_u128(2);
        let current = ExecutionPosition {
            seq: 7,
            id: high_uuid,
        };

        assert_eq!(
            current.max(ExecutionPosition {
                seq: 8,
                id: low_uuid
            }),
            ExecutionPosition {
                seq: 8,
                id: low_uuid
            }
        );
        assert_eq!(
            current.max(ExecutionPosition {
                seq: 7,
                id: low_uuid
            }),
            current
        );
    }

    #[test]
    fn paired_execution_table_reset_starts_one_new_generation() {
        // Pins: reset recovery is available only when both disposable
        // execution tables change identity inside the same ClickHouse database.
        let previous = execution_upgrade_head(1, 2, 3);
        let current = ExecutionClickHouseIdentities {
            database_uuid: Uuid::from_u128(1),
            run_table_uuid: Uuid::from_u128(4),
            task_table_uuid: Uuid::from_u128(5),
        };

        assert!(
            execution_reset_starts_new_generation(&previous, current, "analytics")
                .expect("a paired table reset should be recoverable")
        );
        assert!(
            !execution_reset_starts_new_generation(
                &previous,
                previous.clickhouse_identities(),
                "analytics"
            )
            .expect("unchanged identities should reuse the current generation")
        );
    }

    #[test]
    fn partial_execution_table_reset_is_rejected() {
        // Pins: recreating only one execution table cannot reset both durable
        // cursors or produce a coherent table generation.
        let previous = execution_upgrade_head(1, 2, 3);
        let current = ExecutionClickHouseIdentities {
            database_uuid: Uuid::from_u128(1),
            run_table_uuid: Uuid::from_u128(4),
            task_table_uuid: Uuid::from_u128(3),
        };

        let error = execution_reset_starts_new_generation(&previous, current, "analytics")
            .expect_err("a partial table reset must be rejected");
        assert_eq!(
            error.to_string(),
            "analytics export contract error: unsafe partial execution analytics reset detected \
             in analytics: execution table UUIDs must change together (dim_execution_runs \
             00000000-0000-0000-0000-000000000002 -> \
             00000000-0000-0000-0000-000000000004, dim_execution_tasks \
             00000000-0000-0000-0000-000000000003 -> \
             00000000-0000-0000-0000-000000000003). Drop exactly both execution tables inside \
             the existing ClickHouse database and restart"
        );
    }

    #[test]
    fn whole_clickhouse_database_recreation_is_rejected() {
        // Pins: replacing the database loses non-execution analytics copies
        // while their Postgres cursors survive, so no new execution generation
        // may make that destructive reset look recovered.
        let previous = execution_upgrade_head(1, 2, 3);
        let current = ExecutionClickHouseIdentities {
            database_uuid: Uuid::from_u128(6),
            run_table_uuid: Uuid::from_u128(4),
            task_table_uuid: Uuid::from_u128(5),
        };

        let error = execution_reset_starts_new_generation(&previous, current, "analytics")
            .expect_err("a whole-database recreation must be rejected");
        assert_eq!(
            error.to_string(),
            "analytics export contract error: unsafe ClickHouse analytics database reset detected \
             for analytics: database UUID changed from 00000000-0000-0000-0000-000000000001 \
             to 00000000-0000-0000-0000-000000000006; persisted cursors for non-execution \
             tables cannot be replayed safely. Do not drop the database; reset execution \
             analytics by dropping exactly dim_execution_runs and dim_execution_tasks together \
             inside the existing database"
        );
    }

    fn execution_upgrade_head(
        database_uuid: u128,
        run_table_uuid: u128,
        task_table_uuid: u128,
    ) -> ExecutionUpgradeHead {
        ExecutionUpgradeHead {
            generation: 1,
            database_uuid: Uuid::from_u128(database_uuid),
            run_table_uuid: Uuid::from_u128(run_table_uuid),
            task_table_uuid: Uuid::from_u128(task_table_uuid),
            export_version_floor: DateTime::<Utc>::UNIX_EPOCH,
        }
    }
}
