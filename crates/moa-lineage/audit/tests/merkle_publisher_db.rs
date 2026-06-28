//! DB-backed Merkle publisher race coverage.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use moa_lineage_audit::{MerkleRootPublisher, RootPublisherConfig, SigningKey};
use object_store::memory::InMemory;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn publisher_race_inserts_one_root_for_one_window_db() -> TestResult<()> {
    // Pins: replicated audit publishers cannot publish duplicate roots for the same partition window.
    let (pool, database_name, cleanup_pool) = isolated_merkle_pool().await?;
    install_merkle_schema(&pool).await?;
    seed_lineage_window(&pool, "race-partition").await?;

    let store = Arc::new(InMemory::new());
    let signing = SigningKey::from_seed("audit-root", [9_u8; 32]);
    let cfg = RootPublisherConfig {
        publish_interval: Duration::from_secs(60),
        max_window_records: 100,
        max_window_age: Duration::from_secs(300),
        ..RootPublisherConfig::default()
    };
    let publisher_a = MerkleRootPublisher::new(
        pool.clone(),
        store.clone(),
        signing.clone(),
        "race-partition",
        cfg.clone(),
    );
    let publisher_b = MerkleRootPublisher::new(pool.clone(), store, signing, "race-partition", cfg);

    let first = tokio::spawn(async move { publisher_a.publish_one_window().await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let second = tokio::spawn(async move { publisher_b.publish_one_window().await });

    let first = first
        .await
        .map_err(|error| test_error(format!("first publisher task should join: {error}")))?;
    let second = second
        .await
        .map_err(|error| test_error(format!("second publisher task should join: {error}")))?;
    assert!(
        first.is_ok() && second.is_ok(),
        "both publishers should complete without surfacing lock contention: first={first:?}, second={second:?}"
    );

    let published_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.audit_roots WHERE storage_partition_id = $1",
    )
    .bind("race-partition")
    .fetch_one(&pool)
    .await
    .map_err(|error| test_error(format!("audit root count should load: {error}")))?;
    assert_eq!(
        published_count, 1,
        "exactly one root should be published for the raced window"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

async fn isolated_merkle_pool() -> TestResult<(sqlx::PgPool, String, sqlx::PgPool)> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@127.0.0.1:10040/moa".to_string());
    let database_name = format!("merkle_test_{}", Uuid::now_v7().simple());
    let cleanup_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .map_err(|error| {
            test_error(format!(
                "test Postgres should be reachable at {database_url}: {error}"
            ))
        })?;
    sqlx::query(&format!(
        "CREATE DATABASE {}",
        quote_identifier(&database_name)
    ))
    .execute(&cleanup_pool)
    .await
    .map_err(|error| test_error(format!("test database should be creatable: {error}")))?;

    let connect_options = PgConnectOptions::from_str(&database_url)
        .map_err(|error| {
            test_error(format!(
                "MOA_DATABASE_URL should be a Postgres URL: {error}"
            ))
        })?
        .database(&database_name);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options)
        .await
        .map_err(|error| {
            test_error(format!(
                "isolated Merkle test database should be reachable: {error}"
            ))
        })?;
    Ok((pool, database_name, cleanup_pool))
}

async fn install_merkle_schema(pool: &sqlx::PgPool) -> sqlx::Result<()> {
    sqlx::query("CREATE SCHEMA analytics").execute(pool).await?;
    sqlx::query(
        r#"
        CREATE TABLE analytics.turn_lineage (
            turn_id UUID NOT NULL,
            session_id UUID NOT NULL,
            user_id TEXT NOT NULL,
            storage_partition_id TEXT NOT NULL,
            ts TIMESTAMPTZ NOT NULL,
            tier SMALLINT NOT NULL DEFAULT 1,
            record_kind SMALLINT NOT NULL,
            payload JSONB NOT NULL,
            answer_text TEXT,
            integrity_hash BYTEA NOT NULL,
            prev_hash BYTEA,
            PRIMARY KEY (turn_id, record_kind, ts)
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE analytics.audit_roots (
            root_id UUID PRIMARY KEY,
            storage_partition_id TEXT NOT NULL,
            window_start TIMESTAMPTZ NOT NULL,
            window_end TIMESTAMPTZ NOT NULL,
            record_count BIGINT NOT NULL,
            merkle_root BYTEA NOT NULL,
            signature BYTEA NOT NULL,
            signing_key_label TEXT NOT NULL,
            s3_object_uri TEXT NOT NULL,
            s3_object_etag TEXT NOT NULL,
            object_lock_mode TEXT NOT NULL,
            retain_until TIMESTAMPTZ NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE analytics.compliance_storage_partition_state (
            storage_partition_id TEXT PRIMARY KEY,
            last_integrity_hash BYTEA,
            last_ts TIMESTAMPTZ,
            record_count BIGINT NOT NULL DEFAULT 0,
            last_root_id UUID
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE FUNCTION analytics.delay_audit_root_insert() RETURNS TRIGGER AS $$
        BEGIN
            PERFORM pg_sleep(0.2);
            RETURN NEW;
        END;
        $$ LANGUAGE plpgsql
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TRIGGER delay_audit_root_insert
        BEFORE INSERT ON analytics.audit_roots
        FOR EACH ROW EXECUTE FUNCTION analytics.delay_audit_root_insert()
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_lineage_window(pool: &sqlx::PgPool, storage_partition_id: &str) -> sqlx::Result<()> {
    let base_ts = Utc::now() - ChronoDuration::minutes(5);
    for idx in 0_i64..3 {
        let leaf = blake3::hash(format!("lineage-{idx}").as_bytes());
        sqlx::query(
            r#"
            INSERT INTO analytics.turn_lineage (
                turn_id, session_id, user_id, storage_partition_id, ts, tier,
                record_kind, payload, integrity_hash, prev_hash
            )
            VALUES ($1, $2, $3, $4, $5, 3, $6, '{}'::jsonb, $7, $8)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .bind("race-user")
        .bind(storage_partition_id)
        .bind(base_ts + ChronoDuration::seconds(idx))
        .bind(idx as i16)
        .bind(leaf.as_bytes().as_slice())
        .bind([idx as u8; 32].as_slice())
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn drop_database(pool: &sqlx::PgPool, database_name: &str) {
    let _ = sqlx::query(
        r#"
        SELECT pg_terminate_backend(pid)
        FROM pg_stat_activity
        WHERE datname = $1
          AND pid <> pg_backend_pid()
        "#,
    )
    .bind(database_name)
    .execute(pool)
    .await;
    let _ = sqlx::query(&format!(
        "DROP DATABASE IF EXISTS {}",
        quote_identifier(database_name)
    ))
    .execute(pool)
    .await;
    pool.close().await;
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn test_error(message: String) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(message))
}
