//! ClickHouse schema bootstrap for the analytics export target.
//!
//! Idempotent `CREATE TABLE IF NOT EXISTS` DDL plus an exact contract check for
//! every table. `events_raw` is a `ReplacingMergeTree` append stream;
//! dimension and fact tables use
//! `ReplacingMergeTree(export_version)` so readers collapse replayed pages with
//! `FINAL`.
//!
//! `CREATE TABLE IF NOT EXISTS` is not a migration: against a table that already
//! exists it is a silent no-op that reports success, so a column added to
//! [`TABLE_DDL`] never reaches a database created before the edit and every
//! later insert fails with `NO_SUCH_COLUMN_IN_TABLE`. The execution dimensions
//! have always been validated against an exact contract for this reason; the
//! other eight tables are validated here against expectations derived from their
//! own DDL, so a drifted table is refused loudly at startup — naming the table
//! and the column — instead of failing silently on every export pass.

use std::collections::HashSet;

use clickhouse::Row;
use clickhouse::sql::Identifier;
use serde::Deserialize;

use super::{
    AnalyticsExporter, EXECUTION_RUN_TABLE, EXECUTION_TASK_TABLE, ExecutionClickHouseIdentities,
    ExportError,
};

impl AnalyticsExporter {
    /// Creates the database and every analytics table when missing.
    pub async fn ensure_clickhouse_schema(&self) -> Result<(), ExportError> {
        self.clickhouse
            .query("CREATE DATABASE IF NOT EXISTS ?")
            .bind(Identifier(&self.database))
            .execute()
            .await?;

        // Read before the DDL runs so the set means "predates this bootstrap".
        // Reading it afterwards would be equally SAFE -- the loop only ever adds
        // tables, so a later read is a superset and validates at least as much
        // -- but it would also re-check tables this call just created, which is
        // work whose result is known.
        let preexisting = self.existing_table_names().await?;
        for ddl in TABLE_DDL {
            self.clickhouse
                .query(ddl)
                .bind(Identifier(&self.database))
                .execute()
                .await?;
        }
        self.validate_declared_table_columns(&preexisting).await?;
        let identities = self.validate_execution_clickhouse_schema().await?;
        self.ensure_execution_dimension_upgrade(identities).await
    }

    /// Names the analytics tables that already exist in the target database.
    async fn existing_table_names(&self) -> Result<HashSet<String>, ExportError> {
        Ok(self
            .clickhouse
            .query("SELECT name FROM system.tables WHERE database = ?")
            .bind(&self.database)
            .fetch_all::<String>()
            .await?
            .into_iter()
            .collect())
    }

    /// Checks each non-execution table that predates this bootstrap against the
    /// columns its own DDL declares, so a table left behind by an earlier
    /// schema is refused at startup rather than failing on every later insert.
    ///
    /// Only pre-existing tables are checked, because that is the precise
    /// statement of the contract: a table this bootstrap just created matches
    /// the DDL by construction, while `CREATE TABLE IF NOT EXISTS` silently
    /// leaves an older one alone. On any restart of a healthy deployment every
    /// table pre-exists and every table is therefore checked.
    ///
    /// The two execution dimensions are excluded because
    /// [`Self::validate_execution_clickhouse_schema`] already holds them to a
    /// stricter contract (engine, keys, and durable table identity) whose reset
    /// guidance is more specific than this generic check.
    async fn validate_declared_table_columns(
        &self,
        preexisting: &HashSet<String>,
    ) -> Result<(), ExportError> {
        for ddl in TABLE_DDL {
            let (table, expected) = declared_table_columns(ddl)?;
            if matches!(table, EXECUTION_RUN_TABLE | EXECUTION_TASK_TABLE)
                || !preexisting.contains(table)
            {
                continue;
            }
            let actual = self.table_column_rows(table).await?;
            validate_table_columns(table, &actual, &expected)?;
        }
        Ok(())
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

    async fn table_column_rows(&self, table: &str) -> Result<Vec<SystemColumnRow>, ExportError> {
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
        let actual = self.table_column_rows(table).await?;
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

/// A table name and the ordered `(column, type)` pairs its DDL declares.
type DeclaredTable<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// Extracts the table name and its ordered `(column, type)` pairs from one
/// [`TABLE_DDL`] entry.
///
/// The DDL is the single declaration of every column, so deriving the
/// expectation from it means a drift check can never fall behind the schema it
/// checks. ClickHouse reports `system.columns.type` byte-identically to the
/// type text used here (including the space in `DateTime64(6, 'UTC')`), so the
/// comparison is exact and needs no normalization.
fn declared_table_columns(ddl: &str) -> Result<DeclaredTable<'_>, ExportError> {
    let malformed = |detail: &str| {
        ExportError::Contract(format!(
            "analytics table DDL is malformed ({detail}); the schema declaration cannot be \
             checked against the live database"
        ))
    };

    let after_marker = ddl
        .find("?.")
        .map(|index| &ddl[index + 2..])
        .ok_or_else(|| malformed("no `?.<table>` name"))?;
    let name_end = after_marker
        .find(|character: char| character.is_whitespace() || character == '(')
        .ok_or_else(|| malformed("table name is not terminated"))?;
    let table = &after_marker[..name_end];

    let body_start = after_marker
        .find('(')
        .ok_or_else(|| malformed("no column list"))?;
    let mut depth = 0usize;
    let mut body_end = None;
    for (offset, character) in after_marker[body_start..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                // Saturating rather than wrapping: a malformed DDL must surface
                // as the contract error below, never as a panic in startup.
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    body_end = Some(body_start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let body_end = body_end.ok_or_else(|| malformed("column list is not closed"))?;
    let body = &after_marker[body_start + 1..body_end];

    // Split on commas at paren depth zero: parameterized types such as
    // `DateTime64(6, 'UTC')` carry their own commas and must stay intact.
    let mut columns = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut spans = Vec::new();
    for (offset, character) in body.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                spans.push(&body[start..offset]);
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }
    spans.push(&body[start..]);

    for span in spans {
        let span = span.trim();
        if span.is_empty() {
            continue;
        }
        let split = span
            .find(char::is_whitespace)
            .ok_or_else(|| malformed(&format!("column declaration {span:?} has no type")))?;
        columns.push((&span[..split], span[split..].trim()));
    }
    if columns.is_empty() {
        return Err(malformed(&format!("table {table} declares no columns")));
    }
    Ok((table, columns))
}

/// Compares a live table's ordered columns against what its DDL declares.
///
/// The error names the table, the position, and both the expected and the
/// observed column so an operator can act on it without re-deriving the
/// difference by hand.
fn validate_table_columns(
    table: &str,
    actual_columns: &[SystemColumnRow],
    expected_columns: &[(&str, &str)],
) -> Result<(), ExportError> {
    let mismatch = actual_columns
        .iter()
        .map(|column| (column.name.as_str(), column.column_type.as_str()))
        .zip(expected_columns.iter().copied())
        .position(|(actual, expected)| actual != expected)
        .or_else(|| {
            (actual_columns.len() != expected_columns.len())
                .then_some(actual_columns.len().min(expected_columns.len()))
        });
    let Some(index) = mismatch else {
        return Ok(());
    };
    let expected = expected_columns.get(index).map_or_else(
        || "<none>".to_string(),
        |(name, kind)| format!("{name} {kind}"),
    );
    let actual = actual_columns.get(index).map_or_else(
        || "<none>".to_string(),
        |column| format!("{} {}", column.name, column.column_type),
    );
    Err(ExportError::Contract(format!(
        "ClickHouse analytics table {table} has drifted from its declared schema at column {}: \
         expected {expected}, found {actual}. `CREATE TABLE IF NOT EXISTS` cannot add a column to \
         an existing table, so this database predates a schema change; migrate or drop and \
         rebuild {table} before exporting",
        index + 1
    )))
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

/// The ten analytics table definitions, database name bound as `?`.
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
    use clickhouse::sql::Identifier;

    use super::{
        EXECUTION_RUN_COLUMNS, EXECUTION_TASK_COLUMNS, ExportError, SystemColumnRow,
        SystemTableContractRow, TABLE_DDL, declared_table_columns,
        validate_execution_table_contract, validate_table_columns,
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

    #[test]
    fn declared_columns_are_parsed_for_every_analytics_table_offline() {
        // Pins: the drift check derives an expectation for all ten tables, so
        // no table can silently opt out of validation by being unparseable.
        let tables: Vec<&str> = TABLE_DDL
            .iter()
            .map(|ddl| {
                let (table, columns) = declared_table_columns(ddl)
                    .unwrap_or_else(|error| panic!("every table DDL must parse, got: {error}"));
                assert!(
                    !columns.is_empty(),
                    "table {table} parsed to an empty column list"
                );
                table
            })
            .collect();

        assert_eq!(
            tables,
            vec![
                "events_raw",
                "dim_sessions",
                "dim_session_agent_context",
                "dim_task_segments",
                "dim_execution_runs",
                "dim_execution_tasks",
                "dim_learning_candidates",
                "dim_experiment_run",
                "turn_fact",
                "tool_call_fact",
            ],
            "the set of declared analytics tables changed"
        );
    }

    /// The live server is the permanent oracle for the DDL parser.
    ///
    /// The offline oracle below is the hand-written execution contract, which
    /// covers only two tables and could in principle be removed. This one cannot
    /// rot: both sides derive from the same DDL, so it stays correct across
    /// schema changes by construction, and it covers every table. Its real job
    /// is the failure the offline tests structurally cannot see -- a ClickHouse
    /// version that changes how a type is rendered in `system.columns`, which
    /// would otherwise turn every startup into a spurious drift refusal.
    #[tokio::test]
    #[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
    async fn declared_columns_equal_the_live_server_for_every_table_docker() {
        if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
            panic!("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test");
        }
        let client = clickhouse::Client::default()
            .with_url(
                std::env::var("MOA_CLICKHOUSE_URL")
                    .unwrap_or_else(|_| "http://localhost:10061".to_string()),
            )
            .with_user(std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string()))
            .with_password(
                std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string()),
            );
        let database = format!("moa_ddl_oracle_{}", uuid::Uuid::now_v7().simple());

        client
            .query("CREATE DATABASE ?")
            .bind(Identifier(&database))
            .execute()
            .await
            .expect("create the isolated oracle database");

        let mut checked = 0usize;
        for ddl in TABLE_DDL {
            client
                .query(ddl)
                .bind(Identifier(&database))
                .execute()
                .await
                .unwrap_or_else(|error| panic!("the server must accept every table DDL: {error}"));

            let (table, declared) = declared_table_columns(ddl).expect("DDL must parse");
            let actual: Vec<SystemColumnRow> = client
                .query(
                    "SELECT name, type AS column_type FROM system.columns \
                     WHERE database = ? AND table = ? ORDER BY position",
                )
                .bind(&database)
                .bind(table)
                .fetch_all()
                .await
                .unwrap_or_else(|error| panic!("read system.columns for {table}: {error}"));

            let observed: Vec<(&str, &str)> = actual
                .iter()
                .map(|column| (column.name.as_str(), column.column_type.as_str()))
                .collect();
            assert_eq!(
                observed, declared,
                "{table}: what the server stores must equal what the DDL parser derives"
            );
            checked += 1;
        }
        assert_eq!(
            checked,
            TABLE_DDL.len(),
            "every declared table must be checked against the server"
        );

        client
            .query("DROP DATABASE IF EXISTS ?")
            .bind(Identifier(&database))
            .execute()
            .await
            .expect("drop the isolated oracle database");
    }

    #[test]
    fn declared_columns_match_the_hand_written_execution_contract_offline() {
        // Pins: the DDL parser is correct, using the curated execution contract
        // as its oracle. This is also the guard that keeps TABLE_DDL and the
        // execution constants from drifting apart -- an edit to one without the
        // other fails here.
        for (table, expected) in [
            ("dim_execution_runs", EXECUTION_RUN_COLUMNS),
            ("dim_execution_tasks", EXECUTION_TASK_COLUMNS),
        ] {
            let ddl = TABLE_DDL
                .iter()
                .find(|ddl| ddl.contains(&format!("?.{table} (")))
                .unwrap_or_else(|| panic!("{table} must have a DDL entry"));

            let (parsed_table, parsed) =
                declared_table_columns(ddl).expect("execution DDL must parse");

            assert_eq!(parsed_table, table);
            assert_eq!(
                parsed,
                expected.to_vec(),
                "DDL-derived columns for {table} disagree with its hand-written contract"
            );
        }
    }

    #[test]
    fn a_table_missing_a_declared_column_is_refused_with_both_values_offline() {
        // Pins: the exact failure this check exists for -- a live table created
        // before a TABLE_DDL column addition. `CREATE TABLE IF NOT EXISTS` is a
        // silent no-op there, so without this the drift surfaces only as
        // NO_SUCH_COLUMN_IN_TABLE on every later insert.
        let ddl = TABLE_DDL
            .iter()
            .find(|ddl| ddl.contains("?.dim_learning_candidates ("))
            .expect("dim_learning_candidates must have a DDL entry");
        let (table, declared) = declared_table_columns(ddl).expect("DDL must parse");
        let dropped = declared[3];
        let stale: Vec<(&str, &str)> = declared
            .iter()
            .copied()
            .filter(|column| *column != dropped)
            .collect();
        assert_eq!(
            stale.len(),
            declared.len() - 1,
            "the stale fixture must be exactly one column short"
        );

        let error = validate_table_columns(table, &system_columns(&stale), &declared)
            .expect_err("a table missing a declared column must be refused");

        let ExportError::Contract(message) = error else {
            panic!("drift must be a contract error");
        };
        assert!(
            message.contains(table) && message.contains(dropped.0),
            "the refusal must name the table and the drifted column, got: {message}"
        );
    }

    #[test]
    fn a_table_missing_its_last_declared_column_is_refused_offline() {
        // Pins the length arm specifically: when the drift is the FINAL column,
        // the ordered zip runs out before it disagrees, so only the length
        // comparison can catch it. Appending a column is the common case, which
        // makes this the likeliest real drift rather than a corner case.
        let ddl = TABLE_DDL
            .iter()
            .find(|ddl| ddl.contains("?.dim_sessions ("))
            .expect("dim_sessions must have a DDL entry");
        let (table, declared) = declared_table_columns(ddl).expect("DDL must parse");
        let (dropped, stale) = declared
            .split_last()
            .expect("the declaration must have columns");
        assert!(
            stale
                .iter()
                .zip(declared.iter())
                .all(|(left, right)| left == right),
            "the stale fixture must be a strict prefix of the declaration"
        );

        let error = validate_table_columns(table, &system_columns(stale), &declared)
            .expect_err("a table missing its trailing column must be refused");

        let ExportError::Contract(message) = error else {
            panic!("drift must be a contract error");
        };
        assert!(
            message.contains(table) && message.contains(dropped.0),
            "the refusal must name the table and the trailing column, got: {message}"
        );
    }

    #[test]
    fn a_table_matching_its_declaration_is_accepted_offline() {
        // Negative control: without this, the test above could be passing
        // because the comparison rejects everything.
        for ddl in TABLE_DDL {
            let (table, declared) = declared_table_columns(ddl).expect("DDL must parse");

            validate_table_columns(table, &system_columns(&declared), &declared).unwrap_or_else(
                |error| panic!("a table matching its own DDL must be accepted, got: {error}"),
            );
        }
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
