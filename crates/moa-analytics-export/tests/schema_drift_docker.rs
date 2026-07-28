//! Live coverage for the ClickHouse schema drift refusal against a real server.
//!
//! Pins the failure the mock-server lanes structurally cannot see: `CREATE
//! TABLE IF NOT EXISTS` against an existing table is a silent no-op that
//! reports success, so a column added to the bootstrap DDL never reaches a
//! database created before that edit, and every later insert fails with
//! `NO_SUCH_COLUMN_IN_TABLE`. Only a real ClickHouse exhibits that no-op; a mock
//! replays whatever the test tells it to.
//!
//! Run with the compose service:
//! `docker compose --profile clickhouse up -d clickhouse`, then
//! `MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 cargo nextest run -p moa-analytics-export \
//!  --profile clickhouse-docker --run-ignored all`.

use clickhouse::Client;
use moa_analytics_export::AnalyticsExporter;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

/// The first column `dim_sessions` declares that the stale fixture below omits.
/// If the declared column order changes, this test fails and prints the real
/// refusal, which is the correct signal rather than a silent pass.
const FIRST_MISSING_COLUMN: &str = "storage_partition_id";

fn clickhouse_client() -> TestResult<(Client, String)> {
    let url = std::env::var("MOA_CLICKHOUSE_URL")
        .unwrap_or_else(|_| "http://localhost:10061".to_string());
    let user = std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string());
    let password = std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string());
    let client = Client::default()
        .with_url(url)
        .with_user(user)
        .with_password(password);
    let database = format!("moa_drift_{}", Uuid::now_v7().simple());
    Ok((client, database))
}

/// A `dim_sessions` from before several columns existed. Deliberately a
/// historical shape, not a copy of the current contract.
async fn create_stale_dim_sessions(client: &Client, database: &str) -> TestResult<()> {
    client
        .query("CREATE DATABASE IF NOT EXISTS ?")
        .bind(clickhouse::sql::Identifier(database))
        .execute()
        .await?;
    client
        .query(
            "CREATE TABLE ?.dim_sessions (
                 session_id UUID,
                 tenant_id UUID,
                 export_version DateTime64(6, 'UTC')
             ) ENGINE = ReplacingMergeTree(export_version)
             ORDER BY (tenant_id, session_id)",
        )
        .bind(clickhouse::sql::Identifier(database))
        .execute()
        .await?;
    Ok(())
}

async fn column_names(client: &Client, database: &str, table: &str) -> TestResult<Vec<String>> {
    Ok(client
        .query(
            "SELECT name FROM system.columns \
             WHERE database = ? AND table = ? ORDER BY position",
        )
        .bind(database)
        .bind(table)
        .fetch_all::<String>()
        .await?)
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn a_database_predating_a_ddl_column_is_refused_at_bootstrap_docker() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }
    let (client, database) = clickhouse_client()?;
    create_stale_dim_sessions(&client, &database).await?;

    // The Postgres pool is never reached: the drift refusal happens before the
    // execution upgrade, which is the only step that touches Postgres. A lazy
    // pool that never connects makes that ordering part of the assertion.
    let pool = PgPoolOptions::new().connect_lazy("postgres://unreachable/unreachable")?;
    let exporter = AnalyticsExporter::with_client(pool, client.clone(), database.clone(), 15, 5000);

    let before = column_names(&client, &database, "dim_sessions").await?;
    assert!(
        !before.contains(&FIRST_MISSING_COLUMN.to_string()),
        "precondition: the stale table must lack {FIRST_MISSING_COLUMN}, found {before:?}"
    );

    let error = exporter
        .ensure_clickhouse_schema()
        .await
        .expect_err("bootstrap against a drifted table must be refused");

    // The no-op is the whole point: bootstrap ran CREATE TABLE IF NOT EXISTS and
    // the column still is not there. Asserting this makes the test a statement
    // about ClickHouse behavior, not only about our error string.
    let after = column_names(&client, &database, "dim_sessions").await?;
    assert_eq!(
        before, after,
        "CREATE TABLE IF NOT EXISTS must not alter the existing table"
    );

    let message = error.to_string();
    assert!(
        message.contains("dim_sessions") && message.contains(FIRST_MISSING_COLUMN),
        "the refusal must name the table and the drifted column, got: {message}"
    );

    client
        .query("DROP DATABASE IF EXISTS ?")
        .bind(clickhouse::sql::Identifier(&database))
        .execute()
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn a_restart_against_its_own_tables_passes_column_validation_docker() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }
    let (client, database) = clickhouse_client()?;
    let pool = PgPoolOptions::new().connect_lazy("postgres://unreachable/unreachable")?;
    let exporter = AnalyticsExporter::with_client(pool, client.clone(), database.clone(), 15, 5000);

    // Negative control for the drift test, in the shape a deployment actually
    // takes. Validation covers the tables that PREDATE a bootstrap, so a first
    // bootstrap checks nothing and would make a weaker control vacuous. The
    // SECOND bootstrap is the real exercise: every table now pre-exists, so
    // every table is validated, and a validator that rejected anything it had
    // itself created would fail right here.
    let first = exporter.ensure_clickhouse_schema().await;
    assert!(
        first.is_err(),
        "precondition: the unreachable Postgres pool must fail the execution upgrade"
    );
    let created = column_names(&client, &database, "dim_sessions").await?;
    assert!(
        created.contains(&FIRST_MISSING_COLUMN.to_string()),
        "precondition: the first bootstrap must have created dim_sessions in full, found {created:?}"
    );

    let error = exporter
        .ensure_clickhouse_schema()
        .await
        .expect_err("the unreachable Postgres pool must fail the execution upgrade again");

    let message = error.to_string();
    assert!(
        !message.contains("drifted from its declared schema"),
        "a restart against tables this bootstrap created must clear validation, got: {message}"
    );

    client
        .query("DROP DATABASE IF EXISTS ?")
        .bind(clickhouse::sql::Identifier(&database))
        .execute()
        .await?;
    Ok(())
}
