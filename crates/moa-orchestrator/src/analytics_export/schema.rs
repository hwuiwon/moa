//! ClickHouse schema bootstrap for the analytics export target.
//!
//! Idempotent `CREATE TABLE IF NOT EXISTS` DDL, run at exporter startup, exactly
//! per `docs/schemas/clickhouse-analytics.md`. `events_raw` is a
//! `ReplacingMergeTree` append stream; the seven `dim_*` tables and the two fact
//! tables are `ReplacingMergeTree(export_version)` so readers collapse the
//! overlap-window duplicates with `FINAL`.

use clickhouse::sql::Identifier;

use super::{AnalyticsExporter, ExportError};

impl AnalyticsExporter {
    /// Creates the database and every analytics table when missing.
    pub async fn ensure_clickhouse_schema(&self) -> Result<(), ExportError> {
        self.clickhouse
            .query("CREATE DATABASE IF NOT EXISTS ?")
            .bind(Identifier(&self.database))
            .execute()
            .await?;

        for ddl in TABLE_DDL {
            self.clickhouse
                .query(ddl)
                .bind(Identifier(&self.database))
                .execute()
                .await?;
        }
        Ok(())
    }
}

/// The eleven analytics table definitions, database name bound as `?`.
const TABLE_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS ?.events_raw (
        event_id UUID,
        session_id UUID,
        tenant_id UUID,
        storage_partition_id String,
        user_id String,
        sequence_num Int64,
        turn_number Int64,
        event_type LowCardinality(String),
        token_count Nullable(Int32),
        payload String,
        ts DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree
    PARTITION BY toYYYYMMDD(ts)
    ORDER BY (tenant_id, session_id, sequence_num)",
    "CREATE TABLE IF NOT EXISTS ?.dim_sessions (
        session_id UUID,
        tenant_id UUID,
        storage_partition_id String,
        user_id String,
        contact_id Nullable(UUID),
        status LowCardinality(String),
        channel LowCardinality(String),
        model Nullable(String),
        title Nullable(String),
        parent_session_id Nullable(UUID),
        total_input_tokens_uncached Int64,
        total_input_tokens_cache_write Int64,
        total_input_tokens_cache_read Int64,
        total_output_tokens Int64,
        total_cost_cents Int64,
        event_count Int64,
        turn_count Int64,
        main_cost_cents Int64,
        auxiliary_cost_cents Int64,
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        completed_at Nullable(DateTime64(6, 'UTC')),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, session_id)",
    "CREATE TABLE IF NOT EXISTS ?.dim_session_agent_context (
        session_id UUID,
        tenant_id UUID,
        agent_id String,
        display_name Nullable(String),
        agent_revision_uid Nullable(UUID),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, session_id)",
    "CREATE TABLE IF NOT EXISTS ?.dim_task_segments (
        segment_id UUID,
        session_id UUID,
        tenant_id UUID,
        storage_partition_id String,
        user_id String,
        segment_index Int32,
        task_summary Nullable(String),
        outcome Nullable(String),
        assessment Nullable(String),
        outcome_confidence Nullable(Float64),
        tools_used Array(String),
        skills_activated Array(String),
        turn_count Int64,
        token_cost Int64,
        started_at DateTime64(6, 'UTC'),
        ended_at Nullable(DateTime64(6, 'UTC')),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, session_id, segment_index)",
    "CREATE TABLE IF NOT EXISTS ?.dim_artifact_run (
        run_uid UUID,
        tenant_id UUID,
        storage_partition_id String,
        user_id String,
        session_id Nullable(UUID),
        revision_uid Nullable(UUID),
        procedure_ref String,
        status LowCardinality(String),
        error Nullable(String),
        started_at Nullable(DateTime64(6, 'UTC')),
        completed_at Nullable(DateTime64(6, 'UTC')),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, run_uid)",
    "CREATE TABLE IF NOT EXISTS ?.dim_artifact_node_run (
        node_run_uid UUID,
        run_uid UUID,
        tenant_id UUID,
        node_id String,
        status LowCardinality(String),
        error Nullable(String),
        started_at Nullable(DateTime64(6, 'UTC')),
        completed_at Nullable(DateTime64(6, 'UTC')),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, run_uid, node_run_uid)",
    "CREATE TABLE IF NOT EXISTS ?.dim_learning_candidates (
        candidate_id UUID,
        tenant_id UUID,
        storage_partition_id String,
        candidate_type LowCardinality(String),
        status LowCardinality(String),
        target_id Nullable(String),
        target_label Nullable(String),
        confidence Nullable(Float64),
        risk_class Nullable(String),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, candidate_id)",
    "CREATE TABLE IF NOT EXISTS ?.dim_experiment_run (
        run_uid UUID,
        tenant_id UUID,
        storage_partition_id String,
        name String,
        target_kind LowCardinality(String),
        status LowCardinality(String),
        score_run_id Nullable(UUID),
        session_id Nullable(UUID),
        error Nullable(String),
        started_at Nullable(DateTime64(6, 'UTC')),
        completed_at Nullable(DateTime64(6, 'UTC')),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, run_uid)",
    "CREATE TABLE IF NOT EXISTS ?.turn_fact (
        tenant_id UUID,
        storage_partition_id String,
        contact_id Nullable(UUID),
        user_id String,
        session_id UUID,
        turn_number Int64,
        finished_at DateTime64(6, 'UTC'),
        model Nullable(String),
        pipeline_ms Nullable(Float64),
        llm_ms Float64,
        tool_ms Float64,
        tool_call_count Int64,
        input_tokens_uncached Int64,
        input_tokens_cache_write Int64,
        input_tokens_cache_read Int64,
        total_input_tokens Int64,
        output_tokens Int64,
        cost_cents Int64,
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, session_id, turn_number)",
    "CREATE TABLE IF NOT EXISTS ?.tool_call_fact (
        tenant_id UUID,
        storage_partition_id String,
        user_id String,
        session_id UUID,
        call_sequence_num Int64,
        turn_number Int64,
        tool_id Nullable(UUID),
        tool_name String,
        success Nullable(Bool),
        duration_ms Nullable(Float64),
        model_tier Nullable(String),
        ts DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, session_id, call_sequence_num)",
];
