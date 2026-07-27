//! Cross-backend parity harness: every catalog dataset queried against the
//! Postgres materialized-view backend and the ClickHouse read-model backend on
//! the same seeded data must return identical cells.
//!
//! The corpus is seeded so that *every* catalog dataset has rows (sessions with
//! multi-turn ToolCall/ToolResult/ToolError/BrainResponse/Error sequences, task
//! segments with and without skills, execution runs + tasks, learning
//! candidates, and experiment runs). The Postgres path refreshes the
//! `analytics.*_fact` matviews (via the store's canonical refresh, the same list
//! `moa-session` uses); the ClickHouse path runs the real exporter
//! (`ensure_clickhouse_schema` + `run_one_pass`) into a per-run isolated CH
//! database. A catalog-driven query battery then runs each dataset's dimensions
//! and measures through both `AnalyticsService::new().query` and
//! `AnalyticsService::clickhouse().query_clickhouse`, and the results are diffed
//! with normalization: rows are canonically sorted by their dimension cells,
//! numbers compared with a relative tolerance, timestamp cells normalized to
//! epoch micros, and `Null == Null`.
//!
//! Run with the compose services (Postgres up, ClickHouse started):
//! `docker compose --profile clickhouse start clickhouse`, then
//! `MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 MOA_DATABASE_URL=... cargo nextest run \
//!  -p moa-orchestrator --run-ignored all -E 'test(analytics_parity)'`.

use chrono::{DateTime, Duration, Utc};
use clickhouse::sql::Identifier;
use clickhouse::{Client, Row};
use moa_analytics::{AnalyticsClickHouseClient, AnalyticsService};
use moa_analytics_export::AnalyticsExporter;
use moa_config::ClickHouseConfig;
use moa_core::types::identifiers::TenantId;
use moa_wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDataset, AnalyticsDimension, AnalyticsFieldKind,
    AnalyticsFieldRole, AnalyticsFilter, AnalyticsFilterOperator, AnalyticsMeasure,
    AnalyticsOrderBy, AnalyticsQueryRequest, AnalyticsQueryResponse, AnalyticsSortDirection,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Relative tolerance for float measure comparison across the two SQL engines.
const FLOAT_REL_TOLERANCE: f64 = 1e-6;

fn clickhouse_config(prefix: &str) -> ClickHouseConfig {
    ClickHouseConfig {
        url: std::env::var("MOA_CLICKHOUSE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10061".to_string()),
        database: format!("{prefix}_{}", Uuid::now_v7().simple()),
        user: Some(std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string())),
        password: Some(
            std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string()),
        ),
        ..ClickHouseConfig::default()
    }
}

fn clickhouse_client(config: &ClickHouseConfig) -> Client {
    let mut client = Client::default().with_url(config.url.trim());
    if let Some(user) = config.user.as_deref() {
        client = client.with_user(user);
    }
    if let Some(password) = config.password.as_deref() {
        client = client.with_password(password);
    }
    client
}

/// Documented parity exceptions: `(dataset_id, field_id, reason)`.
///
/// A tuple here means the field is knowingly served differently by the
/// ClickHouse backend and the battery skips comparing it. Each is a verified
/// semantic gap, listed in the run report.
const PARITY_EXCEPTIONS: &[(&str, &str, &str)] = &[
    // The Postgres `tool_call_analytics` view exposes `finished_at` as the actual
    // ToolResult/ToolError event timestamp (`COALESCE(result.timestamp,
    // error.timestamp)`). The ClickHouse `tool_call_fact` table persists only
    // `called_at` (`ts`) and `duration_ms`, so the dialect reconstructs
    // `finished_at = called_at + duration_ms`. The two agree only when a tool's
    // self-reported `duration_ms` equals the wall-clock gap between its ToolCall
    // and ToolResult events; they diverge otherwise. CH cannot serve the exact
    // PG value without also exporting the result-event timestamp.
    (
        "tool_calls",
        "finished_at",
        "CH reconstructs finished_at = called_at + reported duration_ms; PG uses the actual \
         ToolResult/ToolError event timestamp",
    ),
];

#[derive(Debug, Row, Deserialize)]
struct ClickHouseColumn {
    name: String,
    column_type: String,
}

#[derive(Debug, Row, Deserialize)]
struct ClickHouseCount {
    row_count: u64,
}

#[derive(Debug, Row, Deserialize)]
struct ClickHouseTableContract {
    #[serde(with = "clickhouse::serde::uuid")]
    table_uuid: Uuid,
    engine: String,
    engine_full: String,
    partition_key: String,
    sorting_key: String,
    primary_key: String,
}

#[derive(Debug, Row, Deserialize)]
struct ClickHouseTableIdentity {
    #[serde(with = "clickhouse::serde::uuid")]
    table_uuid: Uuid,
}

#[derive(Debug, Row, Deserialize)]
struct ClickHouseDatabaseIdentity {
    #[serde(with = "clickhouse::serde::uuid")]
    database_uuid: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, sqlx::FromRow)]
struct ExecutionBootstrapGeneration {
    generation: i64,
    database_uuid: Uuid,
    run_table_uuid: Uuid,
    task_table_uuid: Uuid,
    stage: String,
    upgrade_version: DateTime<Utc>,
    export_version_floor: DateTime<Utc>,
    run_high_water_seq: i64,
    run_high_water_id: Uuid,
    task_high_water_seq: i64,
    task_high_water_id: Uuid,
    run_page_seq: i64,
    run_page_id: Uuid,
    task_page_seq: i64,
    task_page_id: Uuid,
    completed_at: Option<DateTime<Utc>>,
}

async fn clickhouse_columns(
    client: &Client,
    database: &str,
    table: &str,
) -> TestResult<Vec<ClickHouseColumn>> {
    Ok(client
        .query(
            "SELECT name, type AS column_type FROM system.columns \
             WHERE database = ? AND table = ? ORDER BY position",
        )
        .bind(database)
        .bind(table)
        .fetch_all()
        .await?)
}

async fn clickhouse_count(client: &Client, database: &str, table: &str) -> TestResult<u64> {
    let row: ClickHouseCount = client
        .query("SELECT count() AS row_count FROM ?.? FINAL")
        .bind(Identifier(database))
        .bind(Identifier(table))
        .fetch_one()
        .await?;
    Ok(row.row_count)
}

async fn clickhouse_table_uuid(client: &Client, database: &str, table: &str) -> TestResult<Uuid> {
    let row: ClickHouseTableIdentity = client
        .query(
            "SELECT uuid AS table_uuid FROM system.tables \
             WHERE database = ? AND name = ?",
        )
        .bind(database)
        .bind(table)
        .fetch_one()
        .await?;
    Ok(row.table_uuid)
}

async fn clickhouse_database_uuid(client: &Client, database: &str) -> TestResult<Uuid> {
    let row: ClickHouseDatabaseIdentity = client
        .query("SELECT uuid AS database_uuid FROM system.databases WHERE name = ?")
        .bind(database)
        .fetch_one()
        .await?;
    Ok(row.database_uuid)
}

async fn execution_bootstrap_generations(
    pool: &PgPool,
) -> TestResult<Vec<ExecutionBootstrapGeneration>> {
    Ok(sqlx::query_as(
        "SELECT generation, database_uuid, run_table_uuid, task_table_uuid, stage, \
                upgrade_version, \
                export_version_floor, run_high_water_seq, run_high_water_id, \
                task_high_water_seq, task_high_water_id, run_page_seq, run_page_id, \
                task_page_seq, task_page_id, completed_at \
         FROM analytics.clickhouse_schema_upgrade_state \
         WHERE upgrade_key = 'execution_dimensions' ORDER BY generation",
    )
    .fetch_all(pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn analytics_parity_all_datasets_docker() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    seed_corpus(&pool, tenant).await?;

    // Postgres backend: refresh the `analytics.*_fact` matviews (and their
    // `session_turn_metrics` dependency) using the store's canonical refresh,
    // which is the exact list `moa-session` refreshes in production and orders
    // `session_turn_metrics` before `turn_fact`.
    test_db
        .store()
        .refresh_analytics_materialized_views()
        .await?;

    // ClickHouse backend: bootstrap schema and run one full export pass into an
    // isolated CH database so concurrent runs cannot collide.
    let config = clickhouse_config("moa_analytics_parity");
    let exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    exporter.ensure_clickhouse_schema().await?;
    exporter.run_one_pass().await?;

    let ch_client = AnalyticsClickHouseClient::connect(&config);
    let pg_service = AnalyticsService::new();
    let ch_service = AnalyticsService::clickhouse();
    let tenant_id = TenantId::from(tenant);

    let catalog = pg_service.catalog();
    let removed_run_mode_field = ["route_", "mode"].concat();
    assert!(
        catalog
            .datasets
            .iter()
            .all(|dataset| !dataset.id.contains("procedure")
                && dataset.fields.iter().all(|field| {
                    field.id != removed_run_mode_field
                        && !matches!(
                            field.id.as_str(),
                            "task_uid" | "source_ref" | "capability_ref"
                        )
                })),
        "execution-only analytics catalog restored a legacy dataset or alias"
    );
    let mut battery_total = 0usize;
    let mut coverage: Vec<String> = Vec::new();

    for dataset in &catalog.datasets {
        // Postgres-only datasets (no ClickHouse source, e.g. citation_precision
        // over the never-exported moa.retrieval_lineage table) cannot have
        // cross-backend parity; the compiler rejects them for ClickHouse.
        if moa_analytics::dialect::clickhouse_from_sql(&dataset.id).is_none() {
            coverage.push(format!("{}: skipped (postgres-only dataset)", dataset.id));
            continue;
        }
        let queries = build_battery(dataset, tenant_id);
        let dims = role_fields(dataset, AnalyticsFieldRole::Dimension).len();
        let measures = role_fields(dataset, AnalyticsFieldRole::Measure).len();
        coverage.push(format!(
            "{}: {} queries ({} dims, {} measures)",
            dataset.id,
            queries.len(),
            dims,
            measures
        ));
        for (label, request) in queries {
            battery_total += 1;
            let pg_result = pg_service.query(&pool, request.clone()).await;
            let ch_result = ch_service
                .query_clickhouse(&ch_client, request.clone())
                .await;
            match (pg_result, ch_result) {
                (Ok(pg_response), Ok(ch_response)) => {
                    diff_responses(&dataset.id, &label, &pg_response, &ch_response)?;
                }
                (Err(pg_error), Err(ch_error)) => {
                    return Err(format!(
                        "parity battery generated an invalid query for {}/{}: pg={pg_error}; ch={ch_error}",
                        dataset.id, label
                    )
                    .into());
                }
                (pg_result, ch_result) => {
                    return Err(format!(
                        "backend availability mismatch for {}/{}: pg={:?} ch={:?}",
                        dataset.id,
                        label,
                        pg_result.map(|response| response.rows.len()),
                        ch_result.map(|response| response.rows.len()),
                    )
                    .into());
                }
            }
        }
    }

    println!("=== analytics parity battery ===");
    for line in &coverage {
        println!("  {line}");
    }
    println!(
        "  TOTAL: {battery_total} queries across {} datasets, {} documented exceptions",
        catalog.datasets.len(),
        PARITY_EXCEPTIONS.len()
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn execution_schema_hard_cutover_requires_reset_on_drift_docker() -> TestResult<()> {
    // Pins: fresh execution dimensions have the exact current schema and keys;
    // an existing incompatible table is rejected without parsing or rewriting rows.
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let config = clickhouse_config("moa_execution_schema_hard_cutover");
    let client = clickhouse_client(&config);
    let exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    exporter.ensure_clickhouse_schema().await?;

    let expected_run_columns = [
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
    let expected_task_columns = [
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
    for (table, expected) in [
        ("dim_execution_runs", expected_run_columns.as_slice()),
        ("dim_execution_tasks", expected_task_columns.as_slice()),
    ] {
        let actual = clickhouse_columns(&client, &config.database, table).await?;
        let actual: Vec<(&str, &str)> = actual
            .iter()
            .map(|column| (column.name.as_str(), column.column_type.as_str()))
            .collect();
        assert_eq!(actual, expected, "{table} fresh schema differs");
    }

    for (table, expected_key) in [
        ("dim_execution_runs", "tenant_id, run_uid"),
        ("dim_execution_tasks", "tenant_id, run_uid, task_id"),
    ] {
        let table_contract: ClickHouseTableContract = client
            .query(
                "SELECT uuid AS table_uuid, engine, engine_full, partition_key, \
                        sorting_key, primary_key FROM system.tables \
                 WHERE database = ? AND name = ?",
            )
            .bind(&config.database)
            .bind(table)
            .fetch_one()
            .await?;
        assert!(!table_contract.table_uuid.is_nil(), "{table} table UUID");
        assert_eq!(
            table_contract.engine, "ReplacingMergeTree",
            "{table} engine"
        );
        assert!(
            table_contract
                .engine_full
                .starts_with("ReplacingMergeTree(export_version) ORDER BY "),
            "{table} version expression: {}",
            table_contract.engine_full
        );
        assert_eq!(table_contract.partition_key, "", "{table} partition key");
        assert_eq!(
            table_contract.sorting_key, expected_key,
            "{table} sorting key"
        );
        assert_eq!(
            table_contract.primary_key, expected_key,
            "{table} primary key"
        );
    }

    client
        .query("DROP TABLE ?.dim_execution_tasks")
        .bind(Identifier(&config.database))
        .execute()
        .await?;
    client
        .query(
            "CREATE TABLE ?.dim_execution_tasks ( \
                 task_id UUID, run_uid UUID, tenant_id UUID, node_id String, item_key String, \
                 capability_ref Nullable(String), export_version DateTime64(6, 'UTC') \
             ) ENGINE = ReplacingMergeTree(export_version) \
             ORDER BY (tenant_id, run_uid, task_id)",
        )
        .bind(Identifier(&config.database))
        .execute()
        .await?;
    client
        .query(
            "INSERT INTO ?.dim_execution_tasks VALUES \
             (?, ?, ?, 'legacy-node', '', 'docs.search:region:version', now64(6))",
        )
        .bind(Identifier(&config.database))
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute()
        .await?;

    let error = exporter
        .ensure_clickhouse_schema()
        .await
        .expect_err("an incompatible execution table must require a ClickHouse reset");
    assert_eq!(
        error.to_string(),
        "analytics export contract error: ClickHouse analytics reset required for \
         dim_execution_tasks: ordered column/type mismatch at position 6: expected task_kind \
         LowCardinality(String), found capability_ref Nullable(String); in-place execution schema \
         changes are unsupported"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        1,
        "hard cutover validation must not copy or rewrite incompatible rows"
    );
    let incompatible_columns =
        clickhouse_columns(&client, &config.database, "dim_execution_tasks").await?;
    assert_eq!(
        incompatible_columns
            .iter()
            .map(|column| (column.name.as_str(), column.column_type.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("task_id", "UUID"),
            ("run_uid", "UUID"),
            ("tenant_id", "UUID"),
            ("node_id", "String"),
            ("item_key", "String"),
            ("capability_ref", "Nullable(String)"),
            ("export_version", "DateTime64(6, 'UTC')"),
        ],
        "hard cutover validation must leave incompatible schema untouched"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn execution_schema_reset_starts_new_generation_and_restores_history_docker() -> TestResult<()>
{
    // Pins: recreating disposable execution tables starts one table-identity-
    // bound bootstrap generation, restores every historical source row, and a
    // replayed startup resumes the completed generation instead of adding one.
    // A partial table reset or whole-database recreation is rejected without a
    // new generation because only the paired execution-table reset is safe.
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    seed_corpus(&pool, tenant).await?;
    let expected_runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_run")
        .fetch_one(&pool)
        .await?;
    let expected_tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_task")
        .fetch_one(&pool)
        .await?;

    let config = clickhouse_config("moa_execution_schema_reset_generation");
    let client = clickhouse_client(&config);
    let exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    exporter.ensure_clickhouse_schema().await?;

    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        u64::try_from(expected_runs)?
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        u64::try_from(expected_tasks)?
    );
    let first_generations = execution_bootstrap_generations(&pool).await?;
    assert_eq!(first_generations.len(), 1);
    let first = &first_generations[0];
    let first_database_uuid = clickhouse_database_uuid(&client, &config.database).await?;
    assert_eq!(first.generation, 1);
    assert_eq!(first.database_uuid, first_database_uuid);
    assert_eq!(first.stage, "complete");
    assert!(first.completed_at.is_some());
    assert_eq!(first.run_page_seq, first.run_high_water_seq);
    assert_eq!(first.run_page_id, first.run_high_water_id);
    assert_eq!(first.task_page_seq, first.task_high_water_seq);
    assert_eq!(first.task_page_id, first.task_high_water_id);
    assert_eq!(
        first.run_table_uuid,
        clickhouse_table_uuid(&client, &config.database, "dim_execution_runs").await?
    );
    assert_eq!(
        first.task_table_uuid,
        clickhouse_table_uuid(&client, &config.database, "dim_execution_tasks").await?
    );

    client
        .query("DROP TABLE ?.dim_execution_runs")
        .bind(Identifier(&config.database))
        .execute()
        .await?;
    let partial_reset = AnalyticsExporter::from_config(pool.clone(), &config);
    let partial_error = partial_reset
        .ensure_clickhouse_schema()
        .await
        .expect_err("a one-table reset must be rejected");
    let partial_run_uuid =
        clickhouse_table_uuid(&client, &config.database, "dim_execution_runs").await?;
    assert_eq!(
        partial_error.to_string(),
        format!(
            "analytics export contract error: unsafe partial execution analytics reset detected \
             in {}: execution table UUIDs must change together (dim_execution_runs {} -> {}, \
             dim_execution_tasks {} -> {}). Drop exactly both execution tables inside the \
             existing ClickHouse database and restart",
            config.database,
            first.run_table_uuid,
            partial_run_uuid,
            first.task_table_uuid,
            first.task_table_uuid
        )
    );
    assert_eq!(
        execution_bootstrap_generations(&pool).await?,
        first_generations,
        "a partial reset must not append a bootstrap generation"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        u64::try_from(expected_tasks)?,
        "partial reset detection must leave the surviving table untouched"
    );

    for table in ["dim_execution_runs", "dim_execution_tasks"] {
        client
            .query("DROP TABLE ?.?")
            .bind(Identifier(&config.database))
            .bind(Identifier(table))
            .execute()
            .await?;
    }

    let restarted = AnalyticsExporter::from_config(pool.clone(), &config);
    restarted.ensure_clickhouse_schema().await?;

    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        u64::try_from(expected_runs)?,
        "new run table must be repopulated from the zero cursor"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        u64::try_from(expected_tasks)?,
        "new task table must be repopulated from the zero cursor"
    );
    let second_generations = execution_bootstrap_generations(&pool).await?;
    assert_eq!(second_generations.len(), 2);
    assert_eq!(second_generations[0], *first);
    let second = &second_generations[1];
    assert_eq!(second.generation, 2);
    assert_eq!(second.database_uuid, first.database_uuid);
    assert_eq!(second.stage, "complete");
    assert!(second.completed_at.is_some());
    assert_ne!(second.run_table_uuid, first.run_table_uuid);
    assert_ne!(second.task_table_uuid, first.task_table_uuid);
    assert_eq!(second.run_page_seq, second.run_high_water_seq);
    assert_eq!(second.run_page_id, second.run_high_water_id);
    assert_eq!(second.task_page_seq, second.task_high_water_seq);
    assert_eq!(second.task_page_id, second.task_high_water_id);
    assert!(second.upgrade_version > first.export_version_floor);
    assert!(second.export_version_floor >= second.upgrade_version);
    assert_eq!(
        second.run_table_uuid,
        clickhouse_table_uuid(&client, &config.database, "dim_execution_runs").await?
    );
    assert_eq!(
        second.task_table_uuid,
        clickhouse_table_uuid(&client, &config.database, "dim_execution_tasks").await?
    );

    let replayed = AnalyticsExporter::from_config(pool.clone(), &config);
    replayed.ensure_clickhouse_schema().await?;
    assert_eq!(
        execution_bootstrap_generations(&pool).await?,
        second_generations,
        "replayed startup must reuse the completed table generation"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        u64::try_from(expected_runs)?
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        u64::try_from(expected_tasks)?
    );

    client
        .query("DROP DATABASE ?")
        .bind(Identifier(&config.database))
        .execute()
        .await?;
    let recreated = AnalyticsExporter::from_config(pool.clone(), &config);
    let database_reset_error = recreated
        .ensure_clickhouse_schema()
        .await
        .expect_err("a whole ClickHouse database recreation must be rejected");
    let recreated_database_uuid = clickhouse_database_uuid(&client, &config.database).await?;
    assert_ne!(recreated_database_uuid, second.database_uuid);
    assert_eq!(
        database_reset_error.to_string(),
        format!(
            "analytics export contract error: unsafe ClickHouse analytics database reset detected \
             for {}: database UUID changed from {} to {}; persisted cursors for non-execution \
             tables cannot be replayed safely. Do not drop the database; reset execution \
             analytics by dropping exactly dim_execution_runs and dim_execution_tasks together \
             inside the existing database",
            config.database, second.database_uuid, recreated_database_uuid
        )
    );
    assert_eq!(
        execution_bootstrap_generations(&pool).await?,
        second_generations,
        "a whole-database recreation must not append a bootstrap generation"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        0,
        "database reset detection must run before execution history is re-exported"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        0,
        "database reset detection must run before execution history is re-exported"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn execution_incremental_high_water_recovery_docker() -> TestResult<()> {
    // Pins: a restarted exporter resumes persisted run/task bounds. Rows moved
    // above those bounds are not lost and appear in the next bounded pass.
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(
        &pool, tenant, session, "chat", "running", "claude", 0, 0, 0, 0, 0, 0,
    )
    .await?;

    let config = clickhouse_config("moa_execution_high_water");
    let first_exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    first_exporter.ensure_clickhouse_schema().await?;

    let run_uid = Uuid::now_v7();
    seed_execution_run_and_tasks(
        &pool,
        tenant,
        run_uid,
        session,
        moa_test_support::fixtures::pg_now() - Duration::seconds(5),
        moa_test_support::fixtures::pg_now(),
    )
    .await?;
    let old_run: (i64, Uuid) = sqlx::query_as(
        "SELECT analytics_change_seq, run_uid FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    let old_task: (i64, Uuid) = sqlx::query_as(
        "SELECT analytics_change_seq, task_id FROM moa.execution_task \
         WHERE run_uid = $1 ORDER BY analytics_change_seq DESC, task_id DESC LIMIT 1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    for (table, high_water) in [
        ("dim_execution_runs", old_run),
        ("dim_execution_tasks", old_task),
    ] {
        sqlx::query(
            "UPDATE analytics.clickhouse_export_state \
             SET pass_high_water_seq = $2, pass_high_water_id = $3, pass_started_at = NOW() \
             WHERE table_name = $1",
        )
        .bind(table)
        .bind(high_water.0)
        .bind(high_water.1)
        .execute(&pool)
        .await?;
    }

    sqlx::query(
        "UPDATE moa.execution_run SET updated_at = updated_at + INTERVAL '1 second' \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_task SET updated_at = updated_at + INTERVAL '1 second' \
         WHERE task_id = $1",
    )
    .bind(old_task.1)
    .execute(&pool)
    .await?;

    let resumed_exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    resumed_exporter.export_execution_dimensions().await?;
    let client = clickhouse_client(&config);
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        0,
        "the run moved above the persisted bound"
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        1,
        "the lower task remains inside the persisted bound"
    );

    let first_run_cursor: (i64, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT cursor_seq, cursor_id, pass_high_water_seq \
         FROM analytics.clickhouse_export_state WHERE table_name = 'dim_execution_runs'",
    )
    .fetch_one(&pool)
    .await?;
    let first_task_cursor: (i64, Uuid, Option<i64>) = sqlx::query_as(
        "SELECT cursor_seq, cursor_id, pass_high_water_seq \
         FROM analytics.clickhouse_export_state WHERE table_name = 'dim_execution_tasks'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!((first_run_cursor.0, first_run_cursor.1), old_run);
    assert_eq!((first_task_cursor.0, first_task_cursor.1), old_task);
    assert_eq!(first_run_cursor.2, None);
    assert_eq!(first_task_cursor.2, None);

    resumed_exporter.export_execution_dimensions().await?;
    let new_run: (i64, Uuid) = sqlx::query_as(
        "SELECT analytics_change_seq, run_uid FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    let new_task: (i64, Uuid) = sqlx::query_as(
        "SELECT analytics_change_seq, task_id FROM moa.execution_task \
         WHERE task_id = $1",
    )
    .bind(old_task.1)
    .fetch_one(&pool)
    .await?;
    assert!(new_run.0 > old_run.0);
    assert!(new_task.0 > old_task.0);
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_runs").await?,
        1
    );
    assert_eq!(
        clickhouse_count(&client, &config.database, "dim_execution_tasks").await?,
        2
    );
    for (table, expected) in [
        ("dim_execution_runs", new_run),
        ("dim_execution_tasks", new_task),
    ] {
        let cursor: (i64, Uuid, Option<i64>) = sqlx::query_as(
            "SELECT cursor_seq, cursor_id, pass_high_water_seq \
             FROM analytics.clickhouse_export_state WHERE table_name = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await?;
        assert_eq!((cursor.0, cursor.1), expected);
        assert_eq!(cursor.2, None);
    }

    Ok(())
}

/// Fields of a dataset with the requested role, in catalog order.
fn role_fields(
    dataset: &AnalyticsDataset,
    role: AnalyticsFieldRole,
) -> Vec<&moa_wire::analytics::AnalyticsField> {
    dataset
        .fields
        .iter()
        .filter(|field| field.role == role)
        .collect()
}

/// Whether `(dataset, field)` is a documented parity exception to skip.
fn is_excepted(dataset_id: &str, field_id: &str) -> bool {
    PARITY_EXCEPTIONS
        .iter()
        .any(|(dataset, field, _)| *dataset == dataset_id && *field == field_id)
}

/// Builds the representative query battery for one dataset:
/// (a) each dimension alone with a `count`; (b) each measure with `sum` and
/// `avg`; (c) one `p95`; (d) one time-range `between` filter; (e) one
/// `ORDER BY` a measure with `LIMIT`.
fn build_battery(
    dataset: &AnalyticsDataset,
    tenant_id: TenantId,
) -> Vec<(String, AnalyticsQueryRequest)> {
    let mut battery = Vec::new();
    let dims = role_fields(dataset, AnalyticsFieldRole::Dimension);
    let measures = role_fields(dataset, AnalyticsFieldRole::Measure);

    let bounded_time_filter = dataset
        .default_time_field
        .as_deref()
        .map(|field| AnalyticsFilter {
            field: field.to_string(),
            operator: AnalyticsFilterOperator::Between,
            value: Some(AnalyticsCell::Json(json!([
                (moa_test_support::fixtures::pg_now() - Duration::days(2)).to_rfc3339(),
                (moa_test_support::fixtures::pg_now() + Duration::days(1)).to_rfc3339(),
            ]))),
        });
    let base = |dataset_id: &str| AnalyticsQueryRequest {
        dataset: dataset_id.to_string(),
        tenant_id: Some(tenant_id),
        dimensions: Vec::new(),
        measures: Vec::new(),
        filters: bounded_time_filter.clone().into_iter().collect(),
        order_by: Vec::new(),
        limit: Some(1000),
    };
    let count_measure = || AnalyticsMeasure {
        field: None,
        aggregation: AnalyticsAggregation::Count,
        alias: Some("row_count".to_string()),
    };

    // (a) each dimension alone + Count.
    for field in &dims {
        if is_excepted(&dataset.id, &field.id) {
            continue;
        }
        let mut request = base(&dataset.id);
        request.dimensions = vec![AnalyticsDimension {
            field: field.id.clone(),
            alias: None,
        }];
        request.measures = vec![count_measure()];
        battery.push((format!("dim[{}]+count", field.id), request));
    }

    // (b) each numeric measure with Sum and Avg.
    for field in &measures {
        if is_excepted(&dataset.id, &field.id) {
            continue;
        }
        for aggregation in [AnalyticsAggregation::Sum, AnalyticsAggregation::Avg] {
            let mut request = base(&dataset.id);
            request.measures = vec![AnalyticsMeasure {
                field: Some(field.id.clone()),
                aggregation,
                alias: Some("agg".to_string()),
            }];
            battery.push((format!("measure[{}]:{:?}", field.id, aggregation), request));
        }
    }

    // (c) one P95 on the first numeric measure.
    if let Some(field) = measures
        .iter()
        .find(|field| !is_excepted(&dataset.id, &field.id))
    {
        let mut request = base(&dataset.id);
        request.measures = vec![AnalyticsMeasure {
            field: Some(field.id.clone()),
            aggregation: AnalyticsAggregation::P95,
            alias: Some("p95".to_string()),
        }];
        battery.push((format!("measure[{}]:P95", field.id), request));
    }

    // (d) one time-range filter on the dataset's default time field.
    if let Some(time_field) = dataset.default_time_field.as_deref() {
        let mut request = base(&dataset.id);
        request.measures = vec![count_measure()];
        let low = (moa_test_support::fixtures::pg_now() - Duration::days(2)).to_rfc3339();
        let high = (moa_test_support::fixtures::pg_now() + Duration::days(1)).to_rfc3339();
        request.filters = vec![AnalyticsFilter {
            field: time_field.to_string(),
            operator: AnalyticsFilterOperator::Between,
            value: Some(AnalyticsCell::Json(json!([low, high]))),
        }];
        battery.push((format!("timefilter[{time_field}]"), request));
    }

    // (e) one ORDER BY a measure + LIMIT. The limit is set above the group count
    // so ORDER BY only reorders (never truncates): the two engines break ties on
    // equal measures differently, and the diff canonically re-sorts anyway, so a
    // truncating limit could drop different tied rows per backend and is avoided
    // deliberately.
    if let (Some(dim), Some(measure)) = (
        dims.iter()
            .find(|field| !is_excepted(&dataset.id, &field.id)),
        measures
            .iter()
            .find(|field| !is_excepted(&dataset.id, &field.id)),
    ) {
        let mut request = base(&dataset.id);
        request.dimensions = vec![AnalyticsDimension {
            field: dim.id.clone(),
            alias: None,
        }];
        request.measures = vec![AnalyticsMeasure {
            field: Some(measure.id.clone()),
            aggregation: AnalyticsAggregation::Sum,
            alias: Some("ordered".to_string()),
        }];
        request.order_by = vec![AnalyticsOrderBy {
            field: "ordered".to_string(),
            direction: AnalyticsSortDirection::Desc,
        }];
        request.limit = Some(1000);
        battery.push((format!("orderby[{}]", measure.id), request));
    }

    battery
}

/// Diffs two analytics responses after normalization, returning `Err` on any
/// structural or cell mismatch with both row sets printed.
fn diff_responses(
    dataset_id: &str,
    label: &str,
    pg: &AnalyticsQueryResponse,
    ch: &AnalyticsQueryResponse,
) -> TestResult<()> {
    if pg.columns.len() != ch.columns.len() {
        return Err(format!(
            "{dataset_id}/{label}: column count differs pg={} ch={}",
            pg.columns.len(),
            ch.columns.len()
        )
        .into());
    }
    for (index, (pg_col, ch_col)) in pg.columns.iter().zip(ch.columns.iter()).enumerate() {
        if pg_col.kind != ch_col.kind {
            return Err(format!(
                "{dataset_id}/{label}: column {index} kind differs pg={:?} ch={:?}",
                pg_col.kind, ch_col.kind
            )
            .into());
        }
    }
    let kinds: Vec<AnalyticsFieldKind> = pg.columns.iter().map(|column| column.kind).collect();
    let dim_indices: Vec<usize> = pg
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.role == AnalyticsFieldRole::Dimension)
        .map(|(index, _)| index)
        .collect();

    let pg_rows = normalized_sorted(&pg.rows, &kinds, &dim_indices);
    let ch_rows = normalized_sorted(&ch.rows, &kinds, &dim_indices);

    if pg_rows.len() != ch_rows.len() {
        return Err(format!(
            "{dataset_id}/{label}: row count differs pg={} ch={}\n  pg={:?}\n  ch={:?}",
            pg_rows.len(),
            ch_rows.len(),
            pg.rows,
            ch.rows
        )
        .into());
    }

    for (row_index, (pg_row, ch_row)) in pg_rows.iter().zip(ch_rows.iter()).enumerate() {
        for (col_index, kind) in kinds.iter().enumerate() {
            let pg_cell = &pg_row[col_index];
            let ch_cell = &ch_row[col_index];
            if !cells_equal(*kind, pg_cell, ch_cell) {
                return Err(format!(
                    "{dataset_id}/{label}: mismatch at row {row_index} col {col_index} ({kind:?}) \
                     pg={pg_cell:?} ch={ch_cell:?}\n  pg_rows={pg_rows:?}\n  ch_rows={ch_rows:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Canonical value used for comparison and sorting, one per cell.
#[derive(Debug, Clone, PartialEq)]
enum NormCell {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// Timestamp normalized to epoch microseconds (backend-independent).
    Micros(i64),
}

/// Normalizes each response row to `NormCell`s and sorts rows by their dimension
/// cells, which are exact across backends and so give a backend-independent
/// order.
fn normalized_sorted(
    rows: &[Vec<AnalyticsCell>],
    kinds: &[AnalyticsFieldKind],
    dim_indices: &[usize],
) -> Vec<Vec<NormCell>> {
    let mut normalized: Vec<Vec<NormCell>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(kinds.iter())
                .map(|(cell, kind)| normalize_cell(*kind, cell))
                .collect()
        })
        .collect();
    normalized.sort_by_key(|row| sort_key(row, dim_indices));
    normalized
}

/// Builds a stable, lexicographically comparable key from a row's dimension
/// cells.
fn sort_key(row: &[NormCell], dim_indices: &[usize]) -> String {
    let mut key = String::new();
    for &index in dim_indices {
        match &row[index] {
            NormCell::Null => key.push_str("\u{0}null"),
            NormCell::Bool(value) => key.push_str(if *value { "b1" } else { "b0" }),
            NormCell::Int(value) => key.push_str(&format!("i{value:020}")),
            NormCell::Micros(value) => key.push_str(&format!("t{value:020}")),
            // Floats are never dimensions in the catalog, but normalize defensively.
            NormCell::Float(value) => key.push_str(&format!("f{value:.9}")),
            NormCell::Text(value) => key.push_str(value),
        }
        key.push('\u{1}');
    }
    key
}

/// Normalizes one cell according to its column kind.
fn normalize_cell(kind: AnalyticsFieldKind, cell: &AnalyticsCell) -> NormCell {
    match cell {
        AnalyticsCell::Null => NormCell::Null,
        AnalyticsCell::Bool(value) => NormCell::Bool(*value),
        AnalyticsCell::Number(number) => match kind {
            AnalyticsFieldKind::Integer => number
                .as_i64()
                .map(NormCell::Int)
                .unwrap_or_else(|| NormCell::Float(number.as_f64().unwrap_or(f64::NAN))),
            _ => NormCell::Float(number.as_f64().unwrap_or(f64::NAN)),
        },
        AnalyticsCell::String(value) => match kind {
            // Both backends render timestamps as RFC3339 strings; convert to
            // epoch micros so formatting differences never fail the diff.
            AnalyticsFieldKind::Timestamp => DateTime::parse_from_rfc3339(value)
                .map(|timestamp| NormCell::Micros(timestamp.timestamp_micros()))
                .unwrap_or_else(|_| NormCell::Text(value.clone())),
            _ => NormCell::Text(value.clone()),
        },
        AnalyticsCell::Json(value) => NormCell::Text(value.to_string()),
    }
}

/// Compares two normalized cells: numbers within a relative tolerance, everything
/// else exact, `Null == Null`.
fn cells_equal(_kind: AnalyticsFieldKind, left: &NormCell, right: &NormCell) -> bool {
    match (left, right) {
        (NormCell::Null, NormCell::Null) => true,
        (NormCell::Bool(a), NormCell::Bool(b)) => a == b,
        (NormCell::Int(a), NormCell::Int(b)) => a == b,
        (NormCell::Micros(a), NormCell::Micros(b)) => a == b,
        (NormCell::Text(a), NormCell::Text(b)) => a == b,
        // Allow integer/float cross-representation and float tolerance.
        (NormCell::Float(a), NormCell::Float(b)) => floats_close(*a, *b),
        (NormCell::Int(a), NormCell::Float(b)) | (NormCell::Float(b), NormCell::Int(a)) => {
            floats_close(*a as f64, *b)
        }
        _ => false,
    }
}

fn floats_close(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    let scale = a.abs().max(b.abs()).max(1.0);
    (a - b).abs() <= FLOAT_REL_TOLERANCE * scale
}

// ---------------------------------------------------------------------------
// Corpus seeding
// ---------------------------------------------------------------------------

/// Seeds a corpus rich enough that every catalog dataset has rows.
async fn seed_corpus(pool: &PgPool, tenant: Uuid) -> TestResult<()> {
    let base = moa_test_support::fixtures::pg_now() - Duration::days(1);

    // Session 1: chat / completed / claude, two turns exercising ToolCall +
    // ToolResult (success) and ToolCall + ToolError, plus a standalone Error.
    let session1 = Uuid::now_v7();
    seed_session(
        pool,
        tenant,
        session1,
        "chat",
        "completed",
        "claude",
        40,
        8,
        5,
        3,
        22,
        17,
    )
    .await?;
    let tool_a = Uuid::now_v7();
    let tool_b = Uuid::now_v7();
    let events1 = vec![
        (
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_a, "tool_name": "search"}}),
            None,
            0,
        ),
        (
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_a, "success": true, "duration_ms": 42.0}}),
            None,
            50,
        ),
        (
            3,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 110.0, "input_tokens_uncached": 20,
                "input_tokens_cache_write": 4, "input_tokens_cache_read": 6, "output_tokens": 9,
                "cost_cents": 12}}),
            Some(31),
            100,
        ),
        (
            4,
            "ToolCall",
            json!({"data": {"tool_id": tool_b, "tool_name": "fetch"}}),
            None,
            1000,
        ),
        (
            5,
            "ToolError",
            json!({"data": {"tool_id": tool_b, "error": "timeout"}}),
            None,
            1030,
        ),
        (
            6,
            "BrainResponse",
            json!({"data": {"model": "claude", "duration_ms": 205.0, "input_tokens_uncached": 30,
                "input_tokens_cache_write": 1, "input_tokens_cache_read": 5, "output_tokens": 11,
                "cost_cents": 15}}),
            Some(44),
            1100,
        ),
        (
            7,
            "Error",
            json!({"data": {"message": "recoverable"}}),
            None,
            1200,
        ),
    ];
    seed_events(pool, tenant, session1, &events1, base).await?;

    // Session 2: slack / running / gpt, one tool turn.
    let session2 = Uuid::now_v7();
    seed_session(
        pool, tenant, session2, "slack", "running", "gpt", 12, 3, 2, 1, 9, 4,
    )
    .await?;
    let tool_c = Uuid::now_v7();
    let events2 = vec![
        (
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_c, "tool_name": "summarize"}}),
            None,
            0,
        ),
        (
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_c, "success": false, "duration_ms": 88.0}}),
            None,
            60,
        ),
        (
            3,
            "BrainResponse",
            json!({"data": {"model": "gpt", "duration_ms": 150.0, "input_tokens_uncached": 12,
                "input_tokens_cache_write": 0, "input_tokens_cache_read": 2, "output_tokens": 5,
                "cost_cents": 7}}),
            Some(18),
            120,
        ),
    ];
    seed_events(pool, tenant, session2, &events2, base + Duration::hours(2)).await?;

    // Session 3: chat / failed / claude, one no-tool turn.
    let session3 = Uuid::now_v7();
    seed_session(
        pool, tenant, session3, "chat", "failed", "claude", 3, 1, 1, 0, 4, 2,
    )
    .await?;
    let events3 = vec![(
        1,
        "BrainResponse",
        json!({"data": {"model": "claude", "duration_ms": 90.0, "input_tokens_uncached": 8,
            "input_tokens_cache_write": 0, "input_tokens_cache_read": 0, "output_tokens": 3,
            "cost_cents": 4}}),
        Some(11),
        0,
    )];
    seed_events(pool, tenant, session3, &events3, base + Duration::hours(4)).await?;

    // Task segments: one with skills (feeds the skills dataset via unnest), one
    // without, varying outcome.
    seed_task_segment(
        pool,
        tenant,
        session1,
        0,
        "resolve billing question",
        Some("success"),
        Some(0.91),
        &["search", "fetch"],
        &["search_docs", "summarize"],
        2,
        29,
        base,
        Some(base + Duration::seconds(3)),
    )
    .await?;
    seed_task_segment(
        pool,
        tenant,
        session2,
        0,
        "summarize thread",
        Some("failure"),
        Some(0.40),
        &["summarize"],
        &[],
        1,
        9,
        base + Duration::hours(2),
        Some(base + Duration::hours(2) + Duration::seconds(2)),
    )
    .await?;

    // Execution run + tasks.
    let run_uid = Uuid::now_v7();
    seed_execution_run_and_tasks(
        pool,
        tenant,
        run_uid,
        session1,
        base,
        base + Duration::seconds(5),
    )
    .await?;

    // Learning candidates.
    seed_learning_candidate(pool, tenant, "skill", "proposed", Some(0.72), "low", base).await?;
    seed_learning_candidate(
        pool,
        tenant,
        "memory",
        "evaluating",
        Some(0.55),
        "medium",
        base + Duration::hours(1),
    )
    .await?;

    // Experiment run (needs a score_run parent for the NOT NULL FK).
    let score_run = Uuid::now_v7();
    seed_score_run(pool, tenant, score_run).await?;
    seed_experiment_run(
        pool,
        tenant,
        session1,
        score_run,
        "billing-eval",
        "completed",
        base,
        Some(base + Duration::seconds(30)),
    )
    .await?;

    Ok(())
}

/// Seeds a session with its required agent-context row (deferred constraint) and
/// explicit denormalized rollup columns so the token/cost/count measures have
/// real values. `total_input_tokens` and `cache_hit_rate` are generated columns
/// and are intentionally not set.
#[allow(clippy::too_many_arguments)]
async fn seed_session(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    channel: &str,
    status: &str,
    model: &str,
    event_count: i64,
    turn_count: i64,
    uncached: i64,
    cache_read: i64,
    output: i64,
    cost_cents: i64,
) -> TestResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, storage_partition_id, user_id, channel, model, status, \
             event_count, turn_count, total_input_tokens_uncached, total_input_tokens_cache_write, \
             total_input_tokens_cache_read, total_output_tokens, total_cost_cents) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 0, $10, $11, $12)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind("user-1")
    .bind(channel)
    .bind(model)
    .bind(status)
    .bind(event_count)
    .bind(turn_count)
    .bind(uncached)
    .bind(cache_read)
    .bind(output)
    .bind(cost_cents)
    .execute(&mut *tx)
    .await?;
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;
    // A resolved (non-null) agent_id, as production populates it. When agent_id
    // is NULL the two backends differ (the exporter coalesces it to '' while the
    // Postgres matview passes NULL through) — a documented NULL-handling gap the
    // realistic corpus avoids by resolving the agent.
    let agent_id = Uuid::parse_str("00000000-0000-4000-8000-0000000ada01")?;
    sqlx::query(
        "INSERT INTO session_agent_context \
             (session_id, storage_partition_id, user_id, agent_id, agent_definition_ref, \
              agent_revision_uid, policy_hash, display_name, policy_snapshot) \
         VALUES ($1, $2, 'user-1', $3, 'agent://system-default', $4, 'test-hash', 'Test Agent', '{}'::jsonb)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind(agent_id)
    .bind(default_revision)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

type SeededEvent<'a> = (i64, &'a str, serde_json::Value, Option<i32>, i64);

async fn seed_events(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    events: &[SeededEvent<'_>],
    base: DateTime<Utc>,
) -> TestResult<()> {
    for (sequence_num, event_type, payload, token_count, offset_ms) in events {
        sqlx::query(
            "INSERT INTO events \
                 (id, session_id, storage_partition_id, user_id, tenant_id, sequence_num, \
                  event_type, payload, token_count, timestamp) \
             VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(session)
        .bind(tenant.to_string())
        .bind(tenant)
        .bind(*sequence_num)
        .bind(*event_type)
        .bind(payload)
        .bind(*token_count)
        .bind(base + Duration::milliseconds(*offset_ms))
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_task_segment(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    segment_index: i32,
    task_summary: &str,
    outcome: Option<&str>,
    outcome_confidence: Option<f64>,
    tools_used: &[&str],
    skills_activated: &[&str],
    turn_count: i64,
    token_cost: i64,
    started_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    let tools: Vec<String> = tools_used.iter().map(|value| value.to_string()).collect();
    let skills: Vec<String> = skills_activated
        .iter()
        .map(|value| value.to_string())
        .collect();
    sqlx::query(
        "INSERT INTO task_segments \
             (id, session_id, storage_partition_id, user_id, tenant_id, segment_index, \
              task_summary, outcome, outcome_confidence, tools_used, skills_activated, turn_count, \
              token_cost, started_at, ended_at) \
         VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
    )
    .bind(Uuid::now_v7())
    .bind(session)
    .bind(tenant.to_string())
    .bind(tenant.to_string())
    .bind(segment_index)
    .bind(task_summary)
    .bind(outcome)
    .bind(outcome_confidence)
    .bind(&tools)
    .bind(&skills)
    .bind(turn_count)
    .bind(token_cost)
    .bind(started_at)
    .bind(ended_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_execution_run_and_tasks(
    pool: &PgPool,
    tenant: Uuid,
    run_uid: Uuid,
    session: Uuid,
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
) -> TestResult<()> {
    let planning_context_uid = Uuid::now_v7();
    let skill_template_revision_uid = Uuid::now_v7();
    let planning_hash = "1".repeat(64);
    let plan_hash = "2".repeat(64);
    sqlx::query(
        "INSERT INTO moa.execution_planning_context \
             (planning_context_uid, tenant_id, session_id, originating_user_sequence_num, \
              originating_user_event_hash, owner_user_id, planning_context_hash, snapshot) \
         VALUES ($1, $2, $3, 1, $4, 'user-1', $4, '{}'::JSONB)",
    )
    .bind(planning_context_uid)
    .bind(tenant)
    .bind(session)
    .bind(&planning_hash)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.execution_run \
             (run_uid, tenant_id, session_id, originating_user_sequence_num, planning_context_uid, \
              planning_context_hash, owner_user_id, goal_contract, initial_plan, active_plan, \
              initial_plan_hash, active_plan_hash, capability_catalog, authorization_envelope, \
              source_provenance, source_kind, skill_template_ref, \
              skill_template_revision_uid, input, status, progress_total_tasks, started_at) \
         VALUES ($1, $2, $3, 1, $4, $5, 'user-1', \
                 '{\"requirements\":[{\"id\":\"r1\"}]}'::JSONB, '{}'::JSONB, '{}'::JSONB, \
                 $6, $6, '{}'::JSONB, '{}'::JSONB, \
                 jsonb_build_object('kind', 'skill_template', \
                    'skill_template_ref', 'skill://billing-flow', \
                    'skill_template_revision_uid', lower($7::TEXT)), \
                 'skill_template', 'skill://billing-flow', $7, '{}'::JSONB, 'queued', 2, $8)",
    )
    .bind(run_uid)
    .bind(tenant)
    .bind(session)
    .bind(planning_context_uid)
    .bind(&planning_hash)
    .bind(&plan_hash)
    .bind(skill_template_revision_uid)
    .bind(started_at)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE moa.execution_run SET status = 'running', updated_at = $2 WHERE run_uid = $1",
    )
    .bind(run_uid)
    .bind(started_at)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status = 'completed', output = '{}'::JSONB, \
             completion_check_results = '[{\"check_id\":\"complete\"}]'::JSONB, \
             terminal_cause = '{\"kind\":\"completion\",\"limit_stop\":null}'::JSONB, \
             terminal_reason = 'completed', \
             terminal_satisfied_requirement_count = 1, terminal_requirement_count = 1, \
             consumed_cost_microusd = 12, consumed_tokens = 34, consumed_tasks = 2, \
             consumed_tool_calls = 3, consumed_retrieved_bytes = 4096, \
             progress_completed_tasks = 2, completed_at = $2, updated_at = $2 \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .bind(completed_at)
    .execute(pool)
    .await?;

    for (node_id, task_kind, capability, offset_ms, cost, tokens, citation) in [
        (
            "collect-input",
            json!({"kind":"capability","reference":{"name":"docs.search","version":"v1"}}),
            true,
            1_000_i64,
            5_i64,
            13_i64,
            json!([{"source":"doc-1"}]),
        ),
        (
            "draft-reply",
            json!({"kind":"output","value":{}}),
            false,
            4_000_i64,
            7_i64,
            21_i64,
            json!([]),
        ),
    ] {
        let task_id = Uuid::now_v7();
        let item_key = if capability { "item-1" } else { "" };
        sqlx::query(
            "INSERT INTO moa.execution_task \
                 (task_id, run_uid, tenant_id, node_id, item_key, plan_revision, status, input, \
                  task_kind, retry_policy, estimate_cost_microusd, estimate_tokens, estimate_tasks, \
                  estimate_tool_calls, estimate_retrieved_bytes, actual_cost_microusd, actual_tokens, \
                  actual_tasks, actual_tool_calls, actual_retrieved_bytes, citations, \
                  started_at, completed_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, 1, 'completed', '{}'::JSONB, $6, \
                     '{\"max_attempts\":1,\"initial_backoff_ms\":0,\"max_backoff_ms\":0}'::JSONB, \
                     $7, $8, 1, 2, 1024, $7, $8, 1, 2, 1024, $9, $10, $11, $11)",
        )
        .bind(task_id)
        .bind(run_uid)
        .bind(tenant)
        .bind(node_id)
        .bind(item_key)
        .bind(task_kind)
        .bind(cost)
        .bind(tokens)
        .bind(citation)
        .bind(started_at)
        .bind(started_at + Duration::milliseconds(offset_ms))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn seed_learning_candidate(
    pool: &PgPool,
    tenant: Uuid,
    candidate_type: &str,
    status: &str,
    confidence: Option<f64>,
    risk_class: &str,
    updated_at: DateTime<Utc>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO learning_candidates \
             (id, tenant_id, storage_partition_id, candidate_type, status, target_id, \
              target_label, payload, confidence, risk_class, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, '{}'::jsonb, $8, $9, $10, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.to_string())
    .bind(tenant.to_string())
    .bind(candidate_type)
    .bind(status)
    .bind(format!("target-{candidate_type}"))
    .bind(format!("Target {candidate_type}"))
    .bind(confidence)
    .bind(risk_class)
    .bind(updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_score_run(pool: &PgPool, tenant: Uuid, score_run: Uuid) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source) \
         VALUES ($1, $2, 'user-1', 'experiment_run')",
    )
    .bind(score_run)
    .bind(tenant.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed_experiment_run(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    score_run: Uuid,
    name: &str,
    status: &str,
    started_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO moa.experiment_run \
             (run_uid, tenant_id, storage_partition_id, user_id, name, target_kind, status, \
              target, variant, score_run_id, session_id, created_by_identity, started_at, \
              completed_at) \
         VALUES ($1, $2, $3, 'user-1', $4, 'agent_loop', $5, '{}'::jsonb, '{}'::jsonb, $6, $7, \
              '{}'::jsonb, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant)
    .bind(tenant.to_string())
    .bind(name)
    .bind(status)
    .bind(score_run)
    .bind(session)
    .bind(started_at)
    .bind(completed_at)
    .execute(pool)
    .await?;
    Ok(())
}
