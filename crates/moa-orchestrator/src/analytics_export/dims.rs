//! Dimension-table export for timestamp-cursored read models and sequence-
//! cursored execution facts. Rows are ReplacingMergeTree upserts keyed by tenant
//! plus primary key, with monotonic export versions selecting the latest copy.
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
    AnalyticsExporter, ExecutionPosition, ExportError, parse_tenant_uuid, record_lag, record_rows,
    record_tenant_skip,
};

const EXECUTION_RUN_EXPORT_SQL: &str = "SELECT analytics_change_seq, run_uid, tenant_id, \
        contact_id, session_id, initial_plan_hash, active_plan_hash, plan_revision, route_mode, \
        route_reason, source_kind, skill_template_ref, skill_template_revision_uid, status, \
        terminal_reason, COALESCE(terminal_requirement_count, \
            CASE WHEN jsonb_typeof(goal_contract -> 'requirements') = 'array' \
                 THEN jsonb_array_length(goal_contract -> 'requirements')::BIGINT ELSE 0 END) \
            AS requirement_count, \
        COALESCE(terminal_satisfied_requirement_count, 0) AS satisfied_requirement_count, \
        CASE WHEN jsonb_typeof(goal_contract -> 'completion_checks') = 'array' \
             THEN jsonb_array_length(goal_contract -> 'completion_checks')::BIGINT ELSE 0 END \
            AS completion_check_count, \
        progress_total_tasks AS logical_task_count, queued_at, started_at, \
        CASE WHEN queued_at IS NULL OR started_at IS NULL THEN NULL::DOUBLE PRECISION \
             ELSE GREATEST(EXTRACT(EPOCH FROM (started_at - queued_at)) * 1000.0, 0.0) END \
            AS queue_to_start_ms, \
        completed_at, CASE WHEN started_at IS NULL OR completed_at IS NULL \
             THEN NULL::DOUBLE PRECISION \
             ELSE GREATEST(EXTRACT(EPOCH FROM (completed_at - started_at)) * 1000.0, 0.0) END \
            AS duration_ms, \
        reserved_cost_microusd, consumed_cost_microusd AS actual_cost_microusd, \
        reserved_tokens, consumed_tokens AS actual_tokens, \
        reserved_tasks, consumed_tasks AS actual_tasks, \
        reserved_tool_calls, consumed_tool_calls AS actual_tool_calls, \
        reserved_retrieved_bytes, consumed_retrieved_bytes AS actual_retrieved_bytes, \
        created_at, updated_at \
     FROM moa.execution_run \
     WHERE (analytics_change_seq, run_uid) > ($1, $2) \
       AND (analytics_change_seq, run_uid) <= ($3, $4) \
     ORDER BY analytics_change_seq, run_uid LIMIT $5";

const EXECUTION_TASK_EXPORT_SQL: &str = "SELECT analytics_change_seq, task_id, run_uid, tenant_id, \
        node_id, item_key, plan_revision, task_kind ->> 'kind' AS task_kind, \
        CASE WHEN task_kind ->> 'kind' = 'capability' \
             THEN task_kind #>> '{reference,name}' ELSE NULL END AS capability_name, \
        CASE WHEN task_kind ->> 'kind' = 'capability' \
             THEN task_kind #>> '{reference,version}' ELSE NULL END AS capability_version, \
        status, CASE WHEN status = 'failed' \
            THEN COALESCE(current_outcome ->> 'class', error ->> 'class') ELSE NULL END \
            AS failure_class, attempt, generation, \
        jsonb_array_length(citations)::BIGINT AS citation_count, \
        CASE WHEN started_at IS NULL THEN NULL::DOUBLE PRECISION \
             ELSE GREATEST(EXTRACT(EPOCH FROM (started_at - created_at)) * 1000.0, 0.0) END \
            AS queue_latency_ms, \
        CASE WHEN completed_at IS NULL THEN NULL::DOUBLE PRECISION \
             ELSE GREATEST(EXTRACT(EPOCH FROM (completed_at - COALESCE(started_at, created_at))) \
                  * 1000.0, 0.0) END AS duration_ms, \
        reserved_cost_microusd, actual_cost_microusd, reserved_tokens, actual_tokens, \
        reserved_tasks, actual_tasks, reserved_tool_calls, actual_tool_calls, \
        reserved_retrieved_bytes, actual_retrieved_bytes, started_at, completed_at, \
        created_at, updated_at \
     FROM moa.execution_task \
     WHERE (analytics_change_seq, task_id) > ($1, $2) \
       AND (analytics_change_seq, task_id) <= ($3, $4) \
     ORDER BY analytics_change_seq, task_id LIMIT $5";

impl AnalyticsExporter {
    /// Exports both execution dimensions under one shared immutable pass fence.
    pub async fn export_execution_dimensions(&self) -> Result<(), ExportError> {
        self.prepare_execution_passes().await?;
        self.export_execution_run_pass().await?;
        self.export_execution_task_pass().await
    }

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

    pub(super) async fn read_execution_run_page(
        &self,
        after: ExecutionPosition,
        high_water: ExecutionPosition,
    ) -> Result<Vec<ExecutionRunReadRow>, ExportError> {
        let mut tx = self.begin_read_txn().await?;
        let rows = sqlx::query_as::<_, ExecutionRunReadRow>(EXECUTION_RUN_EXPORT_SQL)
            .bind(after.seq)
            .bind(after.id)
            .bind(high_water.seq)
            .bind(high_water.id)
            .bind(self.batch_rows)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows)
    }

    pub(super) async fn read_execution_task_page(
        &self,
        after: ExecutionPosition,
        high_water: ExecutionPosition,
    ) -> Result<Vec<ExecutionTaskReadRow>, ExportError> {
        let mut tx = self.begin_read_txn().await?;
        let rows = sqlx::query_as::<_, ExecutionTaskReadRow>(EXECUTION_TASK_EXPORT_SQL)
            .bind(after.seq)
            .bind(after.id)
            .bind(high_water.seq)
            .bind(high_water.id)
            .bind(self.batch_rows)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows)
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

/// `dim_execution_runs` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DimExecutionRunRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub contact_id: Option<Uuid>,
    #[serde(with = "clickhouse::serde::uuid")]
    pub session_id: Uuid,
    pub initial_plan_hash: String,
    pub active_plan_hash: String,
    pub plan_revision: u64,
    pub route_mode: String,
    pub route_reason: String,
    pub source_kind: String,
    pub skill_template_ref: Option<String>,
    #[serde(with = "clickhouse::serde::uuid::option")]
    pub skill_template_revision_uid: Option<Uuid>,
    pub status: String,
    pub terminal_reason: Option<String>,
    pub requirement_count: u64,
    pub satisfied_requirement_count: u64,
    pub completion_check_count: u64,
    pub logical_task_count: u64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub queued_at: Option<DateTime<Utc>>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub started_at: Option<DateTime<Utc>>,
    pub queue_to_start_ms: Option<f64>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros::option")]
    pub completed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub reserved_cost_microusd: u64,
    pub actual_cost_microusd: u64,
    pub reserved_tokens: u64,
    pub actual_tokens: u64,
    pub reserved_tasks: u64,
    pub actual_tasks: u64,
    pub reserved_tool_calls: u64,
    pub actual_tool_calls: u64,
    pub reserved_retrieved_bytes: u64,
    pub actual_retrieved_bytes: u64,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    pub export_version: DateTime<Utc>,
}

/// `dim_execution_tasks` row.
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DimExecutionTaskRow {
    #[serde(with = "clickhouse::serde::uuid")]
    pub task_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub run_uid: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub tenant_id: Uuid,
    pub node_id: String,
    pub item_key: String,
    pub task_kind: String,
    pub capability_name: Option<String>,
    pub capability_version: Option<String>,
    pub plan_revision: u64,
    pub status: String,
    pub failure_class: Option<String>,
    pub attempt: u32,
    pub generation: u64,
    pub citation_count: u64,
    pub queue_latency_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub reserved_cost_microusd: u64,
    pub actual_cost_microusd: u64,
    pub reserved_tokens: u64,
    pub actual_tokens: u64,
    pub reserved_tasks: u64,
    pub actual_tasks: u64,
    pub reserved_tool_calls: u64,
    pub actual_tool_calls: u64,
    pub reserved_retrieved_bytes: u64,
    pub actual_retrieved_bytes: u64,
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

#[derive(Debug, Clone, FromRow)]
pub(super) struct ExecutionRunReadRow {
    pub(super) analytics_change_seq: i64,
    pub(super) run_uid: Uuid,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    session_id: Uuid,
    initial_plan_hash: String,
    active_plan_hash: String,
    plan_revision: i64,
    route_mode: String,
    route_reason: String,
    source_kind: String,
    skill_template_ref: Option<String>,
    skill_template_revision_uid: Option<Uuid>,
    status: String,
    terminal_reason: Option<String>,
    requirement_count: i64,
    satisfied_requirement_count: i64,
    completion_check_count: i64,
    logical_task_count: i64,
    queued_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    queue_to_start_ms: Option<f64>,
    completed_at: Option<DateTime<Utc>>,
    duration_ms: Option<f64>,
    reserved_cost_microusd: i64,
    actual_cost_microusd: i64,
    reserved_tokens: i64,
    actual_tokens: i64,
    reserved_tasks: i64,
    actual_tasks: i64,
    reserved_tool_calls: i64,
    actual_tool_calls: i64,
    reserved_retrieved_bytes: i64,
    actual_retrieved_bytes: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ExecutionRunReadRow {
    pub(super) fn into_clickhouse(
        self,
        export_version: DateTime<Utc>,
    ) -> Result<DimExecutionRunRow, ExportError> {
        Ok(DimExecutionRunRow {
            run_uid: self.run_uid,
            tenant_id: self.tenant_id,
            contact_id: self.contact_id,
            session_id: self.session_id,
            initial_plan_hash: self.initial_plan_hash,
            active_plan_hash: self.active_plan_hash,
            plan_revision: nonnegative_u64("plan_revision", self.plan_revision)?,
            route_mode: self.route_mode,
            route_reason: self.route_reason,
            source_kind: self.source_kind,
            skill_template_ref: self.skill_template_ref,
            skill_template_revision_uid: self.skill_template_revision_uid,
            status: self.status,
            terminal_reason: self.terminal_reason,
            requirement_count: nonnegative_u64("requirement_count", self.requirement_count)?,
            satisfied_requirement_count: nonnegative_u64(
                "satisfied_requirement_count",
                self.satisfied_requirement_count,
            )?,
            completion_check_count: nonnegative_u64(
                "completion_check_count",
                self.completion_check_count,
            )?,
            logical_task_count: nonnegative_u64("logical_task_count", self.logical_task_count)?,
            queued_at: self.queued_at,
            started_at: self.started_at,
            queue_to_start_ms: self.queue_to_start_ms,
            completed_at: self.completed_at,
            duration_ms: self.duration_ms,
            reserved_cost_microusd: nonnegative_u64(
                "reserved_cost_microusd",
                self.reserved_cost_microusd,
            )?,
            actual_cost_microusd: nonnegative_u64(
                "actual_cost_microusd",
                self.actual_cost_microusd,
            )?,
            reserved_tokens: nonnegative_u64("reserved_tokens", self.reserved_tokens)?,
            actual_tokens: nonnegative_u64("actual_tokens", self.actual_tokens)?,
            reserved_tasks: nonnegative_u64("reserved_tasks", self.reserved_tasks)?,
            actual_tasks: nonnegative_u64("actual_tasks", self.actual_tasks)?,
            reserved_tool_calls: nonnegative_u64("reserved_tool_calls", self.reserved_tool_calls)?,
            actual_tool_calls: nonnegative_u64("actual_tool_calls", self.actual_tool_calls)?,
            reserved_retrieved_bytes: nonnegative_u64(
                "reserved_retrieved_bytes",
                self.reserved_retrieved_bytes,
            )?,
            actual_retrieved_bytes: nonnegative_u64(
                "actual_retrieved_bytes",
                self.actual_retrieved_bytes,
            )?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            export_version,
        })
    }
}

#[derive(Debug, Clone, FromRow)]
pub(super) struct ExecutionTaskReadRow {
    pub(super) analytics_change_seq: i64,
    pub(super) task_id: Uuid,
    run_uid: Uuid,
    tenant_id: Uuid,
    node_id: String,
    item_key: String,
    task_kind: String,
    capability_name: Option<String>,
    capability_version: Option<String>,
    plan_revision: i64,
    status: String,
    failure_class: Option<String>,
    attempt: i32,
    generation: i64,
    citation_count: i64,
    queue_latency_ms: Option<f64>,
    duration_ms: Option<f64>,
    reserved_cost_microusd: i64,
    actual_cost_microusd: i64,
    reserved_tokens: i64,
    actual_tokens: i64,
    reserved_tasks: i64,
    actual_tasks: i64,
    reserved_tool_calls: i64,
    actual_tool_calls: i64,
    reserved_retrieved_bytes: i64,
    actual_retrieved_bytes: i64,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ExecutionTaskReadRow {
    pub(super) fn into_clickhouse(
        self,
        export_version: DateTime<Utc>,
    ) -> Result<DimExecutionTaskRow, ExportError> {
        Ok(DimExecutionTaskRow {
            task_id: self.task_id,
            run_uid: self.run_uid,
            tenant_id: self.tenant_id,
            node_id: self.node_id,
            item_key: self.item_key,
            task_kind: self.task_kind,
            capability_name: self.capability_name,
            capability_version: self.capability_version,
            plan_revision: nonnegative_u64("plan_revision", self.plan_revision)?,
            status: self.status,
            failure_class: self.failure_class,
            attempt: u32::try_from(self.attempt).map_err(|_| {
                ExportError::Contract(format!(
                    "execution task attempt must be nonnegative, got {}",
                    self.attempt
                ))
            })?,
            generation: nonnegative_u64("generation", self.generation)?,
            citation_count: nonnegative_u64("citation_count", self.citation_count)?,
            queue_latency_ms: self.queue_latency_ms,
            duration_ms: self.duration_ms,
            reserved_cost_microusd: nonnegative_u64(
                "reserved_cost_microusd",
                self.reserved_cost_microusd,
            )?,
            actual_cost_microusd: nonnegative_u64(
                "actual_cost_microusd",
                self.actual_cost_microusd,
            )?,
            reserved_tokens: nonnegative_u64("reserved_tokens", self.reserved_tokens)?,
            actual_tokens: nonnegative_u64("actual_tokens", self.actual_tokens)?,
            reserved_tasks: nonnegative_u64("reserved_tasks", self.reserved_tasks)?,
            actual_tasks: nonnegative_u64("actual_tasks", self.actual_tasks)?,
            reserved_tool_calls: nonnegative_u64("reserved_tool_calls", self.reserved_tool_calls)?,
            actual_tool_calls: nonnegative_u64("actual_tool_calls", self.actual_tool_calls)?,
            reserved_retrieved_bytes: nonnegative_u64(
                "reserved_retrieved_bytes",
                self.reserved_retrieved_bytes,
            )?,
            actual_retrieved_bytes: nonnegative_u64(
                "actual_retrieved_bytes",
                self.actual_retrieved_bytes,
            )?,
            started_at: self.started_at,
            completed_at: self.completed_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            export_version,
        })
    }
}

fn nonnegative_u64(field: &str, value: i64) -> Result<u64, ExportError> {
    u64::try_from(value).map_err(|_| {
        ExportError::Contract(format!(
            "execution analytics field {field} must be nonnegative, got {value}"
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::{EXECUTION_RUN_EXPORT_SQL, EXECUTION_TASK_EXPORT_SQL};

    #[test]
    fn execution_export_sql_is_normalized_and_sequence_bounded() {
        // Pins: execution export reads V337's normalized bounded fields and
        // immutable sequence fence; raw prose and compatibility aliases never
        // cross the analytics boundary.
        assert!(
            EXECUTION_RUN_EXPORT_SQL.contains("(analytics_change_seq, run_uid) <= ($3, $4)"),
            "{EXECUTION_RUN_EXPORT_SQL}"
        );
        assert!(
            EXECUTION_RUN_EXPORT_SQL.contains("skill_template_revision_uid")
                && EXECUTION_RUN_EXPORT_SQL.contains("reserved_retrieved_bytes")
                && EXECUTION_RUN_EXPORT_SQL.contains("actual_retrieved_bytes"),
            "{EXECUTION_RUN_EXPORT_SQL}"
        );
        assert!(
            EXECUTION_TASK_EXPORT_SQL.contains("(analytics_change_seq, task_id) <= ($3, $4)")
                && EXECUTION_TASK_EXPORT_SQL.contains("capability_name")
                && EXECUTION_TASK_EXPORT_SQL.contains("capability_version")
                && EXECUTION_TASK_EXPORT_SQL
                    .contains("COALESCE(current_outcome ->> 'class', error ->> 'class')"),
            "{EXECUTION_TASK_EXPORT_SQL}"
        );
        for forbidden in [
            "procedure",
            "source_ref",
            "capability_ref",
            "error ->> 'message'",
            "terminal_gaps",
            "cancellation_reason",
        ] {
            assert!(
                !EXECUTION_RUN_EXPORT_SQL.contains(forbidden)
                    && !EXECUTION_TASK_EXPORT_SQL.contains(forbidden),
                "forbidden execution analytics field {forbidden}"
            );
        }
        assert!(
            !EXECUTION_TASK_EXPORT_SQL.contains("task_uid"),
            "canonical task_id must be used"
        );
    }
}
