//! ClickHouse schema bootstrap for the analytics export target.
//!
//! Idempotent `CREATE TABLE IF NOT EXISTS` DDL plus an exact contract check for
//! execution dimensions. `events_raw` is a `ReplacingMergeTree` append stream;
//! dimension and fact tables use
//! `ReplacingMergeTree(export_version)` so readers collapse replayed pages with
//! `FINAL`.

use clickhouse::Row;
use clickhouse::sql::Identifier;
use serde::Deserialize;

use super::{AnalyticsExporter, ExecutionClickHouseIdentities, ExportError};

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
        let identities = self.validate_execution_clickhouse_schema().await?;
        self.ensure_execution_dimension_upgrade(identities).await
    }

    pub(super) async fn validate_execution_clickhouse_schema(
        &self,
    ) -> Result<ExecutionClickHouseIdentities, ExportError> {
        let database_uuid = self
            .clickhouse
            .query("SELECT uuid AS database_uuid FROM system.databases WHERE name = ?")
            .bind(&self.database)
            .fetch_one::<SystemDatabaseIdentityRow>()
            .await?
            .database_uuid;
        if database_uuid.is_nil() {
            return Err(ExportError::Contract(format!(
                "ClickHouse analytics database {} has no durable UUID; execution reset recovery requires an Atomic database",
                self.database
            )));
        }
        let run_table_uuid = self
            .validate_execution_schema(
                "dim_execution_runs",
                EXECUTION_RUN_COLUMNS,
                "tenant_id, run_uid",
            )
            .await?;
        let task_table_uuid = self
            .validate_execution_schema(
                "dim_execution_tasks",
                EXECUTION_TASK_COLUMNS,
                "tenant_id, run_uid, task_id",
            )
            .await?;
        Ok(ExecutionClickHouseIdentities {
            database_uuid,
            run_table_uuid,
            task_table_uuid,
        })
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

    async fn validate_execution_schema(
        &self,
        table: &str,
        expected: &[(&str, &str)],
        expected_key: &str,
    ) -> Result<uuid::Uuid, ExportError> {
        let actual = self.execution_column_rows(table).await?;
        let keys = self
            .clickhouse
            .query(
                "SELECT uuid AS table_uuid, engine, engine_full, partition_key, \
                        sorting_key, primary_key FROM system.tables \
                 WHERE database = ? AND name = ?",
            )
            .bind(&self.database)
            .bind(table)
            .fetch_one::<SystemTableContractRow>()
            .await?;
        validate_execution_table_contract(table, &actual, &keys, expected, expected_key)?;
        Ok(keys.table_uuid)
    }
}

#[derive(Debug, Row, Deserialize)]
struct SystemDatabaseIdentityRow {
    #[serde(with = "clickhouse::serde::uuid")]
    database_uuid: uuid::Uuid,
}

#[derive(Debug, Row, Deserialize)]
struct SystemColumnRow {
    name: String,
    column_type: String,
}

#[derive(Debug, Row, Deserialize)]
struct SystemTableContractRow {
    #[serde(with = "clickhouse::serde::uuid")]
    table_uuid: uuid::Uuid,
    engine: String,
    engine_full: String,
    partition_key: String,
    sorting_key: String,
    primary_key: String,
}

fn validate_execution_table_contract(
    table: &str,
    actual_columns: &[SystemColumnRow],
    actual_table: &SystemTableContractRow,
    expected_columns: &[(&str, &str)],
    expected_key: &str,
) -> Result<(), ExportError> {
    if actual_table.table_uuid.is_nil() {
        return Err(reset_required_error(
            table,
            "table UUID is nil; reset recovery requires an Atomic table identity".to_string(),
        ));
    }
    if actual_table.engine != "ReplacingMergeTree" {
        return Err(reset_required_error(
            table,
            format!(
                "engine mismatch: expected \"ReplacingMergeTree\", found {:?}",
                actual_table.engine
            ),
        ));
    }
    let expected_engine_prefix = "ReplacingMergeTree(export_version) ORDER BY ";
    if !actual_table.engine_full.starts_with(expected_engine_prefix) {
        return Err(reset_required_error(
            table,
            format!(
                "version expression mismatch: expected export_version, found engine definition {:?}",
                actual_table.engine_full
            ),
        ));
    }
    if !actual_table.partition_key.is_empty() {
        return Err(reset_required_error(
            table,
            format!(
                "partition key mismatch: expected an empty partition key, found {:?}",
                actual_table.partition_key
            ),
        ));
    }
    let mismatch = actual_columns
        .iter()
        .map(|column| (column.name.as_str(), column.column_type.as_str()))
        .zip(expected_columns.iter().copied())
        .position(|(actual, expected)| actual != expected);
    let mismatch = mismatch.or_else(|| {
        (actual_columns.len() != expected_columns.len())
            .then_some(actual_columns.len().min(expected_columns.len()))
    });
    if let Some(index) = mismatch {
        let expected = expected_columns.get(index).map_or_else(
            || "<none>".to_string(),
            |(name, kind)| format!("{name} {kind}"),
        );
        let actual = actual_columns.get(index).map_or_else(
            || "<none>".to_string(),
            |column| format!("{} {}", column.name, column.column_type),
        );
        return Err(reset_required_error(
            table,
            format!(
                "ordered column/type mismatch at position {}: expected {expected}, found {actual}",
                index + 1
            ),
        ));
    }
    if actual_table.sorting_key != expected_key {
        return Err(reset_required_error(
            table,
            format!(
                "sorting key mismatch: expected {expected_key:?}, found {:?}",
                actual_table.sorting_key
            ),
        ));
    }
    if actual_table.primary_key != expected_key {
        return Err(reset_required_error(
            table,
            format!(
                "primary key mismatch: expected {expected_key:?}, found {:?}",
                actual_table.primary_key
            ),
        ));
    }
    Ok(())
}

fn reset_required_error(table: &str, mismatch: String) -> ExportError {
    ExportError::Contract(format!(
        "ClickHouse analytics reset required for {table}: {mismatch}; in-place execution schema changes are unsupported"
    ))
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
    use super::{
        EXECUTION_RUN_COLUMNS, EXECUTION_TASK_COLUMNS, ExportError, SystemColumnRow,
        SystemTableContractRow, TABLE_DDL, validate_execution_table_contract,
    };

    #[test]
    fn execution_clickhouse_contract_has_only_normalized_columns() {
        // Pins: the final execution run dimension preserves typed source
        // provenance, terminal evidence, coverage, cost, and latency without
        // route prose or a redundant mode.
        assert_eq!(
            EXECUTION_RUN_COLUMNS,
            &[
                ("run_uid", "UUID"),
                ("tenant_id", "UUID"),
                ("contact_id", "Nullable(UUID)"),
                ("session_id", "UUID"),
                ("initial_plan_hash", "String"),
                ("active_plan_hash", "String"),
                ("plan_revision", "UInt64"),
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
            ]
        );

        let task: std::collections::BTreeMap<_, _> =
            EXECUTION_TASK_COLUMNS.iter().copied().collect();

        assert_eq!(task.get("item_key"), Some(&"String"));
        assert_eq!(task.get("plan_revision"), Some(&"UInt64"));
        assert!(task.contains_key("task_id"));
        for forbidden in [
            "route_rationale",
            "route_reason",
            "task_uid",
            "source_ref",
            "capability_ref",
            "error",
            "storage_partition_id",
            "user_id",
        ] {
            assert!(
                !EXECUTION_RUN_COLUMNS
                    .iter()
                    .any(|(name, _)| *name == forbidden)
                    && !task.contains_key(forbidden),
                "forbidden execution dimension column {forbidden}"
            );
        }

        let ddl = TABLE_DDL.join("\n");
        for forbidden in [
            "route_rationale",
            "route_reason",
            "task_uid",
            "source_ref",
            "capability_ref",
        ] {
            assert!(!ddl.contains(forbidden));
        }
    }

    #[test]
    fn execution_clickhouse_contract_accepts_exact_columns_and_keys() {
        // Pins: a freshly created execution table passes the same ordered
        // column/type and key validation used at exporter startup.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = exact_table_contract("tenant_id, run_uid, task_id");

        validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect("the exact current execution-task contract should be accepted");
    }

    #[test]
    fn execution_clickhouse_contract_requires_reset_for_extra_route_rationale() {
        // Pins: a pre-cutover run dimension is rejected precisely; startup must
        // never remove, reorder, or copy its legacy column in place.
        let mut columns = system_columns(EXECUTION_RUN_COLUMNS);
        columns.insert(
            7,
            SystemColumnRow {
                name: "route_rationale".to_string(),
                column_type: "String".to_string(),
            },
        );
        let table = exact_table_contract("tenant_id, run_uid");

        let error = validate_execution_table_contract(
            "dim_execution_runs",
            &columns,
            &table,
            EXECUTION_RUN_COLUMNS,
            "tenant_id, run_uid",
        )
        .expect_err("a legacy route-rationale column must require an explicit reset");
        let ExportError::Contract(message) = error else {
            panic!("schema drift should return ExportError::Contract");
        };
        assert_eq!(
            message,
            "ClickHouse analytics reset required for dim_execution_runs: ordered column/type \
             mismatch at position 8: expected source_kind LowCardinality(String), found \
             route_rationale String; in-place execution schema changes are unsupported"
        );
    }

    #[test]
    fn execution_clickhouse_contract_requires_reset_for_wrong_sorting_key() {
        // Pins: exact columns cannot hide a stale sorting key because it changes
        // the table's identity and replacement semantics.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = SystemTableContractRow {
            sorting_key: "tenant_id, run_uid, capability_ref".to_string(),
            primary_key: "tenant_id, run_uid, capability_ref".to_string(),
            ..exact_table_contract("tenant_id, run_uid, task_id")
        };

        let error = validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect_err("a stale execution-task sorting key must require an explicit reset");
        let ExportError::Contract(message) = error else {
            panic!("schema drift should return ExportError::Contract");
        };
        assert_eq!(
            message,
            "ClickHouse analytics reset required for dim_execution_tasks: sorting key mismatch: \
             expected \"tenant_id, run_uid, task_id\", found \"tenant_id, run_uid, \
             capability_ref\"; in-place execution schema changes are unsupported"
        );
    }

    #[test]
    fn execution_clickhouse_contract_requires_non_nil_table_uuid() {
        // Pins: the table identity persisted by reset recovery must identify an
        // Atomic ClickHouse table, not the nil UUID used by identity-less tables.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = SystemTableContractRow {
            table_uuid: uuid::Uuid::nil(),
            ..exact_table_contract("tenant_id, run_uid, task_id")
        };

        let error = validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect_err("a nil table UUID must require an explicit reset");
        assert_contract_message(
            error,
            "ClickHouse analytics reset required for dim_execution_tasks: table UUID is nil; \
             reset recovery requires an Atomic table identity; in-place execution schema \
             changes are unsupported",
        );
    }

    #[test]
    fn execution_clickhouse_contract_requires_replacing_merge_tree_engine() {
        // Pins: matching columns and keys do not make a plain MergeTree safe for
        // replay because it cannot collapse repeated export versions.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = SystemTableContractRow {
            engine: "MergeTree".to_string(),
            engine_full: "MergeTree ORDER BY (tenant_id, run_uid, task_id)".to_string(),
            ..exact_table_contract("tenant_id, run_uid, task_id")
        };

        let error = validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect_err("a non-replacing engine must require an explicit reset");
        assert_contract_message(
            error,
            "ClickHouse analytics reset required for dim_execution_tasks: engine mismatch: \
             expected \"ReplacingMergeTree\", found \"MergeTree\"; in-place execution schema \
             changes are unsupported",
        );
    }

    #[test]
    fn execution_clickhouse_contract_requires_export_version_expression() {
        // Pins: replacement must be ordered by export_version; an unversioned
        // or differently versioned table can retain stale replay rows.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = SystemTableContractRow {
            engine_full: "ReplacingMergeTree(updated_at) ORDER BY (tenant_id, run_uid, task_id) \
                          SETTINGS index_granularity = 8192"
                .to_string(),
            ..exact_table_contract("tenant_id, run_uid, task_id")
        };

        let error = validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect_err("a wrong replacement version must require an explicit reset");
        assert_contract_message(
            error,
            "ClickHouse analytics reset required for dim_execution_tasks: version expression \
             mismatch: expected export_version, found engine definition \
             \"ReplacingMergeTree(updated_at) ORDER BY (tenant_id, run_uid, task_id) SETTINGS \
             index_granularity = 8192\"; in-place execution schema changes are unsupported",
        );
    }

    #[test]
    fn execution_clickhouse_contract_requires_empty_partition_key() {
        // Pins: the execution DDL has no partitioning; matching columns and
        // keys cannot hide a different physical deletion/merge contract.
        let columns = system_columns(EXECUTION_TASK_COLUMNS);
        let table = SystemTableContractRow {
            partition_key: "toYYYYMM(created_at)".to_string(),
            ..exact_table_contract("tenant_id, run_uid, task_id")
        };

        let error = validate_execution_table_contract(
            "dim_execution_tasks",
            &columns,
            &table,
            EXECUTION_TASK_COLUMNS,
            "tenant_id, run_uid, task_id",
        )
        .expect_err("a partitioned execution table must require an explicit reset");
        assert_contract_message(
            error,
            "ClickHouse analytics reset required for dim_execution_tasks: partition key \
             mismatch: expected an empty partition key, found \"toYYYYMM(created_at)\"; \
             in-place execution schema changes are unsupported",
        );
    }

    fn exact_table_contract(key: &str) -> SystemTableContractRow {
        SystemTableContractRow {
            table_uuid: uuid::Uuid::from_u128(1),
            engine: "ReplacingMergeTree".to_string(),
            engine_full: format!(
                "ReplacingMergeTree(export_version) ORDER BY ({key}) SETTINGS \
                 index_granularity = 8192"
            ),
            partition_key: String::new(),
            sorting_key: key.to_string(),
            primary_key: key.to_string(),
        }
    }

    fn assert_contract_message(error: ExportError, expected: &str) {
        let ExportError::Contract(message) = error else {
            panic!("schema drift should return ExportError::Contract");
        };
        assert_eq!(message, expected);
    }

    fn system_columns(contract: &[(&str, &str)]) -> Vec<SystemColumnRow> {
        contract
            .iter()
            .map(|(name, column_type)| SystemColumnRow {
                name: (*name).to_string(),
                column_type: (*column_type).to_string(),
            })
            .collect()
    }
}
