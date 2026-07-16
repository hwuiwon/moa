//! ClickHouse schema bootstrap for the analytics export target.
//!
//! Idempotent `CREATE TABLE IF NOT EXISTS` DDL plus the durable Task 11 upgrade
//! of Task 9's execution dimensions. `events_raw` is a `ReplacingMergeTree`
//! append stream; dimension and fact tables use
//! `ReplacingMergeTree(export_version)` so readers collapse replayed pages with
//! `FINAL`.

use std::collections::BTreeMap;

use clickhouse::Row;
use clickhouse::sql::Identifier;
use serde::Deserialize;

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
        self.ensure_execution_dimension_upgrade().await
    }

    pub(super) async fn upgrade_execution_clickhouse_schema(&self) -> Result<(), ExportError> {
        self.upgrade_execution_run_schema().await?;
        self.upgrade_execution_task_schema().await?;
        self.validate_execution_schema("dim_execution_runs", EXECUTION_RUN_COLUMNS)
            .await?;
        self.validate_execution_schema("dim_execution_tasks", EXECUTION_TASK_COLUMNS)
            .await
    }

    async fn upgrade_execution_run_schema(&self) -> Result<(), ExportError> {
        let mut columns = self.execution_columns("dim_execution_runs").await?;
        self.rename_column_if_needed(
            "dim_execution_runs",
            &columns,
            "required_count",
            "requirement_count",
        )
        .await?;
        columns = self.execution_columns("dim_execution_runs").await?;

        self.repair_nullable_column(
            "dim_execution_runs",
            &columns,
            "session_id",
            "toUUID('00000000-0000-0000-0000-000000000000')",
            "UUID",
        )
        .await?;
        self.repair_nullable_column(
            "dim_execution_runs",
            &columns,
            "route_reason",
            "''",
            "LowCardinality(String)",
        )
        .await?;
        self.modify_column_if_needed("dim_execution_runs", &columns, "plan_revision", "UInt64")
            .await?;

        let additions = [
            ("contact_id", "Nullable(UUID)", "NULL"),
            ("initial_plan_hash", "String", "''"),
            ("active_plan_hash", "String", "''"),
            ("route_mode", "LowCardinality(String)", "'run'"),
            ("skill_template_ref", "Nullable(String)", "NULL"),
            ("skill_template_revision_uid", "Nullable(UUID)", "NULL"),
            ("requirement_count", "UInt64", "0"),
            ("satisfied_requirement_count", "UInt64", "0"),
            ("completion_check_count", "UInt64", "0"),
            ("queued_at", "Nullable(DateTime64(6, 'UTC'))", "NULL"),
            ("started_at", "Nullable(DateTime64(6, 'UTC'))", "NULL"),
            ("queue_to_start_ms", "Nullable(Float64)", "NULL"),
            ("completed_at", "Nullable(DateTime64(6, 'UTC'))", "NULL"),
            ("duration_ms", "Nullable(Float64)", "NULL"),
            ("reserved_tasks", "UInt64", "0"),
            ("actual_tasks", "UInt64", "0"),
            ("reserved_tool_calls", "UInt64", "0"),
            ("actual_tool_calls", "UInt64", "0"),
            ("reserved_retrieved_bytes", "UInt64", "0"),
            ("actual_retrieved_bytes", "UInt64", "0"),
            (
                "created_at",
                "DateTime64(6, 'UTC')",
                "toDateTime64(0, 6, 'UTC')",
            ),
            (
                "updated_at",
                "DateTime64(6, 'UTC')",
                "toDateTime64(0, 6, 'UTC')",
            ),
        ];
        for (name, column_type, default) in additions {
            self.add_column_if_missing("dim_execution_runs", &columns, name, column_type, default)
                .await?;
        }
        for name in [
            "storage_partition_id",
            "user_id",
            "source_ref",
            "plan_hash",
            "required_count",
            "satisfied_count",
            "error",
        ] {
            self.drop_column_if_present("dim_execution_runs", &columns, name)
                .await?;
        }
        self.reorder_execution_columns("dim_execution_runs", EXECUTION_RUN_COLUMNS)
            .await?;
        Ok(())
    }

    async fn upgrade_execution_task_schema(&self) -> Result<(), ExportError> {
        let mut columns = self.execution_columns("dim_execution_tasks").await?;
        self.rename_column_if_needed("dim_execution_tasks", &columns, "task_uid", "task_id")
            .await?;
        columns = self.execution_columns("dim_execution_tasks").await?;

        self.repair_nullable_column("dim_execution_tasks", &columns, "item_key", "''", "String")
            .await?;
        self.modify_column_if_needed("dim_execution_tasks", &columns, "plan_revision", "UInt64")
            .await?;

        let additions = [
            ("task_kind", "LowCardinality(String)", "'capability'"),
            ("capability_name", "Nullable(String)", "NULL"),
            ("capability_version", "Nullable(String)", "NULL"),
            ("failure_class", "Nullable(String)", "NULL"),
            ("queue_latency_ms", "Nullable(Float64)", "NULL"),
            ("duration_ms", "Nullable(Float64)", "NULL"),
            ("reserved_tasks", "UInt64", "0"),
            ("actual_tasks", "UInt64", "0"),
            ("reserved_tool_calls", "UInt64", "0"),
            ("actual_tool_calls", "UInt64", "0"),
            ("reserved_retrieved_bytes", "UInt64", "0"),
            ("actual_retrieved_bytes", "UInt64", "0"),
            ("started_at", "Nullable(DateTime64(6, 'UTC'))", "NULL"),
            ("completed_at", "Nullable(DateTime64(6, 'UTC'))", "NULL"),
            (
                "created_at",
                "DateTime64(6, 'UTC')",
                "toDateTime64(0, 6, 'UTC')",
            ),
            (
                "updated_at",
                "DateTime64(6, 'UTC')",
                "toDateTime64(0, 6, 'UTC')",
            ),
        ];
        for (name, column_type, default) in additions {
            self.add_column_if_missing("dim_execution_tasks", &columns, name, column_type, default)
                .await?;
        }
        for name in ["task_uid", "capability_ref", "error"] {
            self.drop_column_if_present("dim_execution_tasks", &columns, name)
                .await?;
        }
        self.reorder_execution_columns("dim_execution_tasks", EXECUTION_TASK_COLUMNS)
            .await?;
        Ok(())
    }

    async fn execution_columns(
        &self,
        table: &str,
    ) -> Result<BTreeMap<String, String>, ExportError> {
        Ok(self
            .execution_column_rows(table)
            .await?
            .into_iter()
            .map(|row| (row.name, row.column_type))
            .collect())
    }

    async fn execution_column_rows(
        &self,
        table: &str,
    ) -> Result<Vec<SystemColumnRow>, ExportError> {
        Ok(self
            .clickhouse
            .query(
                "SELECT name, type AS column_type FROM system.columns \
                 WHERE database = ? AND table = ? ORDER BY position",
            )
            .bind(&self.database)
            .bind(table)
            .fetch_all::<SystemColumnRow>()
            .await?)
    }

    async fn rename_column_if_needed(
        &self,
        table: &str,
        columns: &BTreeMap<String, String>,
        old: &str,
        new: &str,
    ) -> Result<(), ExportError> {
        if !columns.contains_key(old) || columns.contains_key(new) {
            return Ok(());
        }
        let sql = "ALTER TABLE ?.? RENAME COLUMN ? TO ? SETTINGS mutations_sync = 2".to_string();
        self.clickhouse
            .query(&sql)
            .bind(Identifier(&self.database))
            .bind(Identifier(table))
            .bind(Identifier(old))
            .bind(Identifier(new))
            .execute()
            .await?;
        Ok(())
    }

    async fn repair_nullable_column(
        &self,
        table: &str,
        columns: &BTreeMap<String, String>,
        name: &str,
        replacement: &str,
        target_type: &str,
    ) -> Result<(), ExportError> {
        let Some(current) = columns.get(name) else {
            return Ok(());
        };
        if current.starts_with("Nullable(") {
            let update = format!(
                "ALTER TABLE ?.? UPDATE ? = {replacement} WHERE isNull(?) \
                 SETTINGS mutations_sync = 2"
            );
            self.clickhouse
                .query(&update)
                .bind(Identifier(&self.database))
                .bind(Identifier(table))
                .bind(Identifier(name))
                .bind(Identifier(name))
                .execute()
                .await?;
        }
        self.modify_column_if_needed(table, columns, name, target_type)
            .await
    }

    async fn modify_column_if_needed(
        &self,
        table: &str,
        columns: &BTreeMap<String, String>,
        name: &str,
        target_type: &str,
    ) -> Result<(), ExportError> {
        if columns
            .get(name)
            .is_none_or(|current| current == target_type)
        {
            return Ok(());
        }
        let sql =
            format!("ALTER TABLE ?.? MODIFY COLUMN ? {target_type} SETTINGS mutations_sync = 2");
        self.clickhouse
            .query(&sql)
            .bind(Identifier(&self.database))
            .bind(Identifier(table))
            .bind(Identifier(name))
            .execute()
            .await?;
        Ok(())
    }

    async fn add_column_if_missing(
        &self,
        table: &str,
        columns: &BTreeMap<String, String>,
        name: &str,
        column_type: &str,
        default: &str,
    ) -> Result<(), ExportError> {
        if columns.contains_key(name) {
            return Ok(());
        }
        let sql = format!(
            "ALTER TABLE ?.? ADD COLUMN IF NOT EXISTS ? {column_type} DEFAULT {default} \
             SETTINGS mutations_sync = 2"
        );
        self.clickhouse
            .query(&sql)
            .bind(Identifier(&self.database))
            .bind(Identifier(table))
            .bind(Identifier(name))
            .execute()
            .await?;
        Ok(())
    }

    async fn drop_column_if_present(
        &self,
        table: &str,
        columns: &BTreeMap<String, String>,
        name: &str,
    ) -> Result<(), ExportError> {
        if !columns.contains_key(name) {
            return Ok(());
        }
        self.clickhouse
            .query("ALTER TABLE ?.? DROP COLUMN IF EXISTS ? SETTINGS mutations_sync = 2")
            .bind(Identifier(&self.database))
            .bind(Identifier(table))
            .bind(Identifier(name))
            .execute()
            .await?;
        Ok(())
    }

    async fn validate_execution_schema(
        &self,
        table: &str,
        expected: &[(&str, &str)],
    ) -> Result<(), ExportError> {
        let actual = self.execution_column_rows(table).await?;
        let actual_contract: Vec<(&str, &str)> = actual
            .iter()
            .map(|row| (row.name.as_str(), row.column_type.as_str()))
            .collect();
        if actual_contract != expected {
            return Err(ExportError::Contract(format!(
                "{table} column contract mismatch: expected {expected:?}, got {actual_contract:?}"
            )));
        }
        Ok(())
    }

    async fn reorder_execution_columns(
        &self,
        table: &str,
        expected: &[(&str, &str)],
    ) -> Result<(), ExportError> {
        let actual = self.execution_column_rows(table).await?;
        let actual_contract: Vec<(&str, &str)> = actual
            .iter()
            .map(|row| (row.name.as_str(), row.column_type.as_str()))
            .collect();
        if actual_contract == expected {
            return Ok(());
        }
        for (index, (name, column_type)) in expected.iter().enumerate() {
            let position = match index.checked_sub(1) {
                None => " FIRST".to_string(),
                Some(previous) => format!(" AFTER {}", expected[previous].0),
            };
            let sql = format!(
                "ALTER TABLE ?.? MODIFY COLUMN ? {column_type}{position} \
                 SETTINGS mutations_sync = 2"
            );
            self.clickhouse
                .query(&sql)
                .bind(Identifier(&self.database))
                .bind(Identifier(table))
                .bind(Identifier(name))
                .execute()
                .await?;
        }
        Ok(())
    }
}

#[derive(Debug, Row, Deserialize)]
struct SystemColumnRow {
    name: String,
    column_type: String,
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
    "CREATE TABLE IF NOT EXISTS ?.dim_execution_runs (
        run_uid UUID,
        tenant_id UUID,
        contact_id Nullable(UUID),
        session_id UUID,
        initial_plan_hash String,
        active_plan_hash String,
        plan_revision UInt64,
        route_mode LowCardinality(String),
        route_reason LowCardinality(String),
        source_kind LowCardinality(String),
        skill_template_ref Nullable(String),
        skill_template_revision_uid Nullable(UUID),
        status LowCardinality(String),
        terminal_reason Nullable(String),
        requirement_count UInt64,
        satisfied_requirement_count UInt64,
        completion_check_count UInt64,
        logical_task_count UInt64,
        queued_at Nullable(DateTime64(6, 'UTC')),
        started_at Nullable(DateTime64(6, 'UTC')),
        queue_to_start_ms Nullable(Float64),
        completed_at Nullable(DateTime64(6, 'UTC')),
        duration_ms Nullable(Float64),
        reserved_cost_microusd UInt64,
        actual_cost_microusd UInt64,
        reserved_tokens UInt64,
        actual_tokens UInt64,
        reserved_tasks UInt64,
        actual_tasks UInt64,
        reserved_tool_calls UInt64,
        actual_tool_calls UInt64,
        reserved_retrieved_bytes UInt64,
        actual_retrieved_bytes UInt64,
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, run_uid)",
    "CREATE TABLE IF NOT EXISTS ?.dim_execution_tasks (
        task_id UUID,
        run_uid UUID,
        tenant_id UUID,
        node_id String,
        item_key String,
        task_kind LowCardinality(String),
        capability_name Nullable(String),
        capability_version Nullable(String),
        plan_revision UInt64,
        status LowCardinality(String),
        failure_class Nullable(String),
        attempt UInt32,
        generation UInt64,
        citation_count UInt64,
        queue_latency_ms Nullable(Float64),
        duration_ms Nullable(Float64),
        reserved_cost_microusd UInt64,
        actual_cost_microusd UInt64,
        reserved_tokens UInt64,
        actual_tokens UInt64,
        reserved_tasks UInt64,
        actual_tasks UInt64,
        reserved_tool_calls UInt64,
        actual_tool_calls UInt64,
        reserved_retrieved_bytes UInt64,
        actual_retrieved_bytes UInt64,
        started_at Nullable(DateTime64(6, 'UTC')),
        completed_at Nullable(DateTime64(6, 'UTC')),
        created_at DateTime64(6, 'UTC'),
        updated_at DateTime64(6, 'UTC'),
        export_version DateTime64(6, 'UTC')
    ) ENGINE = ReplacingMergeTree(export_version)
    ORDER BY (tenant_id, run_uid, task_id)",
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

const EXECUTION_RUN_COLUMNS: &[(&str, &str)] = &[
    ("run_uid", "UUID"),
    ("tenant_id", "UUID"),
    ("contact_id", "Nullable(UUID)"),
    ("session_id", "UUID"),
    ("initial_plan_hash", "String"),
    ("active_plan_hash", "String"),
    ("plan_revision", "UInt64"),
    ("route_mode", "LowCardinality(String)"),
    ("route_reason", "LowCardinality(String)"),
    ("source_kind", "LowCardinality(String)"),
    ("skill_template_ref", "Nullable(String)"),
    ("skill_template_revision_uid", "Nullable(UUID)"),
    ("status", "LowCardinality(String)"),
    ("terminal_reason", "Nullable(String)"),
    ("requirement_count", "UInt64"),
    ("satisfied_requirement_count", "UInt64"),
    ("completion_check_count", "UInt64"),
    ("logical_task_count", "UInt64"),
    ("queued_at", "Nullable(DateTime64(6, 'UTC'))"),
    ("started_at", "Nullable(DateTime64(6, 'UTC'))"),
    ("queue_to_start_ms", "Nullable(Float64)"),
    ("completed_at", "Nullable(DateTime64(6, 'UTC'))"),
    ("duration_ms", "Nullable(Float64)"),
    ("reserved_cost_microusd", "UInt64"),
    ("actual_cost_microusd", "UInt64"),
    ("reserved_tokens", "UInt64"),
    ("actual_tokens", "UInt64"),
    ("reserved_tasks", "UInt64"),
    ("actual_tasks", "UInt64"),
    ("reserved_tool_calls", "UInt64"),
    ("actual_tool_calls", "UInt64"),
    ("reserved_retrieved_bytes", "UInt64"),
    ("actual_retrieved_bytes", "UInt64"),
    ("created_at", "DateTime64(6, 'UTC')"),
    ("updated_at", "DateTime64(6, 'UTC')"),
    ("export_version", "DateTime64(6, 'UTC')"),
];

const EXECUTION_TASK_COLUMNS: &[(&str, &str)] = &[
    ("task_id", "UUID"),
    ("run_uid", "UUID"),
    ("tenant_id", "UUID"),
    ("node_id", "String"),
    ("item_key", "String"),
    ("task_kind", "LowCardinality(String)"),
    ("capability_name", "Nullable(String)"),
    ("capability_version", "Nullable(String)"),
    ("plan_revision", "UInt64"),
    ("status", "LowCardinality(String)"),
    ("failure_class", "Nullable(String)"),
    ("attempt", "UInt32"),
    ("generation", "UInt64"),
    ("citation_count", "UInt64"),
    ("queue_latency_ms", "Nullable(Float64)"),
    ("duration_ms", "Nullable(Float64)"),
    ("reserved_cost_microusd", "UInt64"),
    ("actual_cost_microusd", "UInt64"),
    ("reserved_tokens", "UInt64"),
    ("actual_tokens", "UInt64"),
    ("reserved_tasks", "UInt64"),
    ("actual_tasks", "UInt64"),
    ("reserved_tool_calls", "UInt64"),
    ("actual_tool_calls", "UInt64"),
    ("reserved_retrieved_bytes", "UInt64"),
    ("actual_retrieved_bytes", "UInt64"),
    ("started_at", "Nullable(DateTime64(6, 'UTC'))"),
    ("completed_at", "Nullable(DateTime64(6, 'UTC'))"),
    ("created_at", "DateTime64(6, 'UTC')"),
    ("updated_at", "DateTime64(6, 'UTC')"),
    ("export_version", "DateTime64(6, 'UTC')"),
];

#[cfg(test)]
mod tests {
    use super::{EXECUTION_RUN_COLUMNS, EXECUTION_TASK_COLUMNS, TABLE_DDL};

    #[test]
    fn execution_clickhouse_contract_has_only_normalized_columns() {
        // Pins: the final execution dimensions use canonical identity,
        // UInt64 plan revisions, non-null session/item keys, and no Task 9
        // compatibility or raw-prose columns.
        let run: std::collections::BTreeMap<_, _> = EXECUTION_RUN_COLUMNS.iter().copied().collect();
        let task: std::collections::BTreeMap<_, _> =
            EXECUTION_TASK_COLUMNS.iter().copied().collect();

        assert_eq!(run.get("session_id"), Some(&"UUID"));
        assert_eq!(run.get("plan_revision"), Some(&"UInt64"));
        assert_eq!(task.get("item_key"), Some(&"String"));
        assert_eq!(task.get("plan_revision"), Some(&"UInt64"));
        assert!(task.contains_key("task_id"));
        for forbidden in [
            "task_uid",
            "source_ref",
            "capability_ref",
            "error",
            "storage_partition_id",
            "user_id",
        ] {
            assert!(
                !run.contains_key(forbidden) && !task.contains_key(forbidden),
                "forbidden execution dimension column {forbidden}"
            );
        }

        let ddl = TABLE_DDL.join("\n");
        assert!(!ddl.contains("task_uid"));
        assert!(!ddl.contains("source_ref"));
        assert!(!ddl.contains("capability_ref"));
    }
}
