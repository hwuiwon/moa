//! Dimension-table export: incremental `updated_at`-cursored pulls of the seven
//! `dim_*` tables. Rows are ReplacingMergeTree upserts keyed by tenant + primary
//! key with `export_version = updated_at`, so a later mutation of the same row
//! supersedes the earlier copy on merge.
//!
//! TEXT tenant ids (`task_segments`, `learning_candidates`) are cast to UUID in
//! Rust; rows whose tenant id does not parse are skipped with a warning and a
//! `moa_analytics_export_skipped_rows_total` increment rather than aborting the
//! batch.

use chrono::{DateTime, Utc};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::{
    AnalyticsExporter, ExportError, parse_tenant_uuid, record_lag, record_rows, record_tenant_skip,
};

impl AnalyticsExporter {
    /// Runs one full incremental pull for a dimension table.
    ///
    /// `R` is the Postgres read row, `C` the ClickHouse insert row; `extract`
    /// returns each row's `(updated_at, primary_key)` cursor position (computed
    /// on the read row so skipped rows still advance the cursor) and `map`
    /// converts a read row into an insert row (returning `None` to skip it).
    async fn export_dim_paged<R, C, X, M>(
        &self,
        table: &'static str,
        sql: &str,
        extract: X,
        map: M,
    ) -> Result<(), ExportError>
    where
        R: for<'r> FromRow<'r, sqlx::postgres::PgRow> + Send + Unpin,
        C: Row + Serialize,
        X: Fn(&R) -> (DateTime<Utc>, Uuid),
        M: Fn(R) -> Option<C>,
    {
        let cursor = self.read_cursor(table).await?;
        let effective = self.effective_lower_bound(cursor);
        let mut after: Option<(DateTime<Utc>, Uuid)> = None;
        let mut last_cursor_ts: Option<DateTime<Utc>> = None;

        loop {
            let (bound_ts, bound_id) = match after {
                Some((ts, id)) => (ts, Some(id)),
                None => (effective, None),
            };
            let mut tx = self.begin_read_txn().await?;
            let rows: Vec<R> = sqlx::query_as::<_, R>(sql)
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
            let last = extract(last_row);

            let ch_rows: Vec<C> = rows
                .into_iter()
                .filter_map(|row| {
                    let mapped = map(row);
                    if mapped.is_none() {
                        record_tenant_skip(table);
                    }
                    mapped
                })
                .collect();

            self.insert_rows(table, &ch_rows).await?;
            record_rows(table, ch_rows.len() as u64);
            self.write_cursor(table, last.0, None).await?;
            last_cursor_ts = Some(last.0);
            after = Some(last);

            if batch_len < self.batch_rows as usize {
                break;
            }
        }

        if let Some(cursor_ts) = last_cursor_ts {
            record_lag(table, cursor_ts);
        }
        Ok(())
    }

    /// Exports `dim_sessions`.
    pub async fn export_dim_sessions(&self) -> Result<(), ExportError> {
        // `main_cost_cents` / `auxiliary_cost_cents` mirror the `session_summary`
        // tier split; the LATERAL aggregate is bounded to the batch's sessions
        // via the `events(session_id)` index.
        const SQL: &str = "SELECT s.id AS session_id, s.tenant_id, s.storage_partition_id, s.user_id, \
                s.contact_id, s.status, COALESCE(s.channel, 'chat') AS channel, s.model, s.title, \
                s.parent_session_id, \
                COALESCE(s.total_input_tokens_uncached, 0)::BIGINT AS total_input_tokens_uncached, \
                COALESCE(s.total_input_tokens_cache_write, 0)::BIGINT AS total_input_tokens_cache_write, \
                COALESCE(s.total_input_tokens_cache_read, 0)::BIGINT AS total_input_tokens_cache_read, \
                COALESCE(s.total_output_tokens, 0)::BIGINT AS total_output_tokens, \
                COALESCE(s.total_cost_cents, 0)::BIGINT AS total_cost_cents, \
                COALESCE(s.event_count, 0)::BIGINT AS event_count, \
                COALESCE(s.turn_count, 0)::BIGINT AS turn_count, \
                COALESCE(tier_costs.main_cost_cents, 0)::BIGINT AS main_cost_cents, \
                COALESCE(tier_costs.auxiliary_cost_cents, 0)::BIGINT AS auxiliary_cost_cents, \
                s.created_at, s.updated_at, s.completed_at, s.updated_at AS export_version \
             FROM sessions s \
             LEFT JOIN LATERAL ( \
                 SELECT \
                     SUM(CASE WHEN COALESCE(e.payload -> 'data' ->> 'model_tier', \
                             CASE WHEN e.event_type = 'Checkpoint' THEN 'auxiliary' ELSE 'main' END) = 'main' \
                         THEN COALESCE((e.payload -> 'data' ->> 'cost_cents')::BIGINT, 0) ELSE 0 END)::BIGINT AS main_cost_cents, \
                     SUM(CASE WHEN COALESCE(e.payload -> 'data' ->> 'model_tier', \
                             CASE WHEN e.event_type = 'Checkpoint' THEN 'auxiliary' ELSE 'main' END) = 'auxiliary' \
                         THEN COALESCE((e.payload -> 'data' ->> 'cost_cents')::BIGINT, 0) ELSE 0 END)::BIGINT AS auxiliary_cost_cents \
                 FROM events e \
                 WHERE e.session_id = s.id AND e.event_type IN ('BrainResponse', 'Checkpoint') \
             ) tier_costs ON TRUE \
             WHERE (s.updated_at > $1 OR (s.updated_at = $1 AND ($2::uuid IS NULL OR s.id > $2))) \
             ORDER BY s.updated_at, s.id LIMIT $3";
        self.export_dim_paged::<DimSessionRow, DimSessionRow, _, _>(
            "dim_sessions",
            SQL,
            |row| (row.updated_at, row.session_id),
            Some,
        )
        .await
    }

    /// Exports `dim_session_agent_context` (insert-only source; `updated_at`
    /// equals `created_at`).
    pub async fn export_dim_session_agent_context(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT session_id, tenant_id, COALESCE(agent_id::text, '') AS agent_id, \
                display_name, agent_revision_uid, created_at, updated_at, \
                updated_at AS export_version \
             FROM session_agent_context \
             WHERE (updated_at > $1 OR (updated_at = $1 AND ($2::uuid IS NULL OR session_id > $2))) \
             ORDER BY updated_at, session_id LIMIT $3";
        self.export_dim_paged::<DimSessionAgentContextRow, DimSessionAgentContextRow, _, _>(
            "dim_session_agent_context",
            SQL,
            |row| (row.updated_at, row.session_id),
            Some,
        )
        .await
    }

    /// Exports `dim_task_segments`; TEXT tenant ids are cast to UUID.
    pub async fn export_dim_task_segments(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT id AS segment_id, session_id, tenant_id, storage_partition_id, \
                COALESCE(user_id, '') AS user_id, segment_index, task_summary, outcome, assessment, \
                outcome_confidence::double precision AS outcome_confidence, \
                COALESCE(tools_used, '{}') AS tools_used, \
                COALESCE(skills_activated, '{}') AS skills_activated, \
                COALESCE(turn_count, 0)::BIGINT AS turn_count, \
                COALESCE(token_cost, 0)::BIGINT AS token_cost, \
                started_at, ended_at, updated_at, updated_at AS export_version \
             FROM task_segments \
             WHERE (updated_at > $1 OR (updated_at = $1 AND ($2::uuid IS NULL OR id > $2))) \
             ORDER BY updated_at, id LIMIT $3";
        self.export_dim_paged::<TaskSegmentReadRow, DimTaskSegmentRow, _, _>(
            "dim_task_segments",
            SQL,
            |row| (row.updated_at, row.segment_id),
            |row| {
                let tenant_id = parse_tenant_uuid(&row.tenant_id)?;
                Some(DimTaskSegmentRow {
                    segment_id: row.segment_id,
                    session_id: row.session_id,
                    tenant_id,
                    storage_partition_id: row.storage_partition_id,
                    user_id: row.user_id,
                    segment_index: row.segment_index,
                    task_summary: row.task_summary,
                    outcome: row.outcome,
                    assessment: row.assessment,
                    outcome_confidence: row.outcome_confidence,
                    tools_used: row.tools_used,
                    skills_activated: row.skills_activated,
                    turn_count: row.turn_count,
                    token_cost: row.token_cost,
                    started_at: row.started_at,
                    ended_at: row.ended_at,
                    updated_at: row.updated_at,
                    export_version: row.export_version,
                })
            },
        )
        .await
    }

    /// Exports `dim_artifact_run`.
    pub async fn export_dim_artifact_run(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT run_uid, tenant_id, COALESCE(storage_partition_id, '') AS storage_partition_id, \
                COALESCE(user_id, '') AS user_id, session_id, revision_uid, procedure_ref, status, error, \
                started_at, completed_at, created_at, updated_at, updated_at AS export_version \
             FROM moa.artifact_run \
             WHERE tenant_id IS NOT NULL \
               AND (updated_at > $1 OR (updated_at = $1 AND ($2::uuid IS NULL OR run_uid > $2))) \
             ORDER BY updated_at, run_uid LIMIT $3";
        self.export_dim_paged::<DimArtifactRunRow, DimArtifactRunRow, _, _>(
            "dim_artifact_run",
            SQL,
            |row| (row.updated_at, row.run_uid),
            Some,
        )
        .await
    }

    /// Exports `dim_artifact_node_run` (tenant falls back to the parent run).
    pub async fn export_dim_artifact_node_run(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT anr.node_run_uid, anr.run_uid, \
                COALESCE(anr.tenant_id, ar.tenant_id) AS tenant_id, anr.node_id, anr.status, \
                anr.error, anr.started_at, anr.completed_at, anr.created_at, anr.updated_at, \
                anr.updated_at AS export_version \
             FROM moa.artifact_node_run anr \
             JOIN moa.artifact_run ar ON ar.run_uid = anr.run_uid \
             WHERE COALESCE(anr.tenant_id, ar.tenant_id) IS NOT NULL \
               AND (anr.updated_at > $1 OR (anr.updated_at = $1 \
                    AND ($2::uuid IS NULL OR anr.node_run_uid > $2))) \
             ORDER BY anr.updated_at, anr.node_run_uid LIMIT $3";
        self.export_dim_paged::<DimArtifactNodeRunRow, DimArtifactNodeRunRow, _, _>(
            "dim_artifact_node_run",
            SQL,
            |row| (row.updated_at, row.node_run_uid),
            Some,
        )
        .await
    }

    /// Exports `dim_learning_candidates`; TEXT tenant ids are cast to UUID.
    pub async fn export_dim_learning_candidates(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT id AS candidate_id, tenant_id, storage_partition_id, \
                candidate_type, status, target_id, target_label, \
                confidence::double precision AS confidence, \
                risk_class, created_at, updated_at, updated_at AS export_version \
             FROM learning_candidates \
             WHERE (updated_at > $1 OR (updated_at = $1 AND ($2::uuid IS NULL OR id > $2))) \
             ORDER BY updated_at, id LIMIT $3";
        self.export_dim_paged::<LearningCandidateReadRow, DimLearningCandidateRow, _, _>(
            "dim_learning_candidates",
            SQL,
            |row| (row.updated_at, row.candidate_id),
            |row| {
                let tenant_id = parse_tenant_uuid(&row.tenant_id)?;
                Some(DimLearningCandidateRow {
                    candidate_id: row.candidate_id,
                    tenant_id,
                    storage_partition_id: row.storage_partition_id,
                    candidate_type: row.candidate_type,
                    status: row.status,
                    target_id: row.target_id,
                    target_label: row.target_label,
                    confidence: row.confidence,
                    risk_class: row.risk_class,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                    export_version: row.export_version,
                })
            },
        )
        .await
    }

    /// Exports `dim_experiment_run`.
    pub async fn export_dim_experiment_run(&self) -> Result<(), ExportError> {
        const SQL: &str = "SELECT run_uid, tenant_id, COALESCE(storage_partition_id, '') AS storage_partition_id, \
                name, target_kind, status, score_run_id, session_id, error, \
                started_at, completed_at, created_at, updated_at, updated_at AS export_version \
             FROM moa.experiment_run \
             WHERE tenant_id IS NOT NULL \
               AND (updated_at > $1 OR (updated_at = $1 AND ($2::uuid IS NULL OR run_uid > $2))) \
             ORDER BY updated_at, run_uid LIMIT $3";
        self.export_dim_paged::<DimExperimentRunRow, DimExperimentRunRow, _, _>(
            "dim_experiment_run",
            SQL,
            |row| (row.updated_at, row.run_uid),
            Some,
        )
        .await
    }
}

/// `dim_sessions` row; field order matches the ClickHouse column order.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct DimSessionRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub user_id: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub contact_id: Option<Uuid>,
    pub status: String,
    pub channel: String,
    pub model: Option<String>,
    pub title: Option<String>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub parent_session_id: Option<Uuid>,
    pub total_input_tokens_uncached: i64,
    pub total_input_tokens_cache_write: i64,
    pub total_input_tokens_cache_read: i64,
    pub total_output_tokens: i64,
    pub total_cost_cents: i64,
    pub event_count: i64,
    pub turn_count: i64,
    pub main_cost_cents: i64,
    pub auxiliary_cost_cents: i64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_session_agent_context` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct DimSessionAgentContextRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub agent_id: String,
    pub display_name: Option<String>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub agent_revision_uid: Option<Uuid>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_artifact_run` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct DimArtifactRunRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub user_id: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub session_id: Option<Uuid>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub revision_uid: Option<Uuid>,
    pub procedure_ref: String,
    pub status: String,
    pub error: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_artifact_node_run` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct DimArtifactNodeRunRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub node_run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub node_id: String,
    pub status: String,
    pub error: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_experiment_run` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize, FromRow)]
pub struct DimExperimentRunRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub name: String,
    pub target_kind: String,
    pub status: String,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub score_run_id: Option<Uuid>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub session_id: Option<Uuid>,
    pub error: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_task_segments` ClickHouse insert row.
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DimTaskSegmentRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub segment_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub user_id: String,
    pub segment_index: i32,
    pub task_summary: Option<String>,
    pub outcome: Option<String>,
    pub assessment: Option<String>,
    pub outcome_confidence: Option<f64>,
    pub tools_used: Vec<String>,
    pub skills_activated: Vec<String>,
    pub turn_count: i64,
    pub token_cost: i64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub started_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// Postgres read row for `task_segments` (TEXT tenant id parsed in Rust).
#[derive(Debug, Clone, FromRow)]
struct TaskSegmentReadRow {
    segment_id: Uuid,
    session_id: Uuid,
    tenant_id: String,
    storage_partition_id: String,
    user_id: String,
    segment_index: i32,
    task_summary: Option<String>,
    outcome: Option<String>,
    assessment: Option<String>,
    outcome_confidence: Option<f64>,
    tools_used: Vec<String>,
    skills_activated: Vec<String>,
    turn_count: i64,
    token_cost: i64,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    export_version: DateTime<Utc>,
}

/// `dim_learning_candidates` ClickHouse insert row.
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DimLearningCandidateRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub candidate_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub storage_partition_id: String,
    pub candidate_type: String,
    pub status: String,
    pub target_id: Option<String>,
    pub target_label: Option<String>,
    pub confidence: Option<f64>,
    pub risk_class: Option<String>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// Postgres read row for `learning_candidates` (TEXT tenant id parsed in Rust).
#[derive(Debug, Clone, FromRow)]
struct LearningCandidateReadRow {
    candidate_id: Uuid,
    tenant_id: String,
    storage_partition_id: String,
    candidate_type: String,
    status: String,
    target_id: Option<String>,
    target_label: Option<String>,
    confidence: Option<f64>,
    risk_class: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    export_version: DateTime<Utc>,
}
