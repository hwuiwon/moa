//! DB-backed coverage for the lineage writer worker.
//!
//! These tests drive the real `spawn_writer` worker against an isolated Postgres
//! database. They pin two durability contracts that have no other integration
//! coverage:
//! 1. graceful shutdown flushes the in-memory batch so pending rows land in
//!    `analytics.turn_lineage`; and
//! 2. a batch whose write fails non-retryably is persisted to
//!    `analytics.lineage_dead_letters` and its journal sequence is left
//!    unacknowledged (so it can be reprocessed).
//!
//! They require `MOA_DATABASE_URL` to point at a reachable Postgres superuser
//! role that can `CREATE DATABASE`; each test creates and drops its own database.

use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use moa_core::{SessionId, StoragePartitionId, TenantId, UserId};
use moa_lineage_core::{
    BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings,
};
use moa_lineage_sink::{MpscSinkConfig, spawn_writer};
use moa_memory_types::MemoryScope;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::mpsc;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn lineage_writer_flush_on_shutdown_drains_pending_rows_db() -> TestResult<()> {
    // Pins: events buffered in the writer's batch (never reaching the size or
    // age flush thresholds) are still drained to Postgres at graceful shutdown.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let journal = tempfile::tempdir()?;

    let config = MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        journal_path: journal.path().to_path_buf(),
        lossy_telemetry: false,
    };

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, pool.clone()).await?;

    let turn_ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for turn_id in &turn_ids {
        tx.send(retrieval_event(*turn_id, "flush-partition"))
            .await
            .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
    }
    // Drop the sender so the channel does not keep blocking the worker; shutdown
    // is what we are exercising.
    drop(tx);

    let stats = handle.shutdown().await?;
    assert_eq!(
        stats.written, 3,
        "all three buffered events must be written by the shutdown drain"
    );

    let written: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.turn_lineage WHERE storage_partition_id = $1",
    )
    .bind("flush-partition")
    .fetch_one(&pool)
    .await?;
    assert_eq!(written, 3, "three lineage rows should be durably persisted");

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn lineage_writer_poison_batch_dead_letters_without_acking_journal_db() -> TestResult<()> {
    // Pins: a batch whose write fails non-retryably is moved to
    // `analytics.lineage_dead_letters`, and its journal sequence is NOT acked.
    // The "not acked" guarantee is proven behaviorally: a fresh writer over the
    // same journal replays the retained row and finally persists it once the
    // write target is healthy again.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let journal = tempfile::tempdir()?;
    let journal_path = journal.path().to_path_buf();

    let config = MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        journal_path: journal_path.clone(),
        lossy_telemetry: false,
    };

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config.clone(), pool.clone()).await?;

    // Poison the write target: dropping the destination table makes the COPY/INSERT
    // fail with `undefined_table` (42P01), which is not a retryable SQLSTATE.
    sqlx::query("DROP TABLE analytics.turn_lineage CASCADE")
        .execute(&pool)
        .await?;

    let turn_id = Uuid::now_v7();
    tx.send(retrieval_event(turn_id, "poison-partition"))
        .await
        .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
    drop(tx);

    let stats = handle.shutdown().await?;
    assert_eq!(
        stats.written, 0,
        "the poison batch must not count as written"
    );

    let dead_letters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dead_letters, 1,
        "the poison batch should be persisted exactly once to dead-letter storage"
    );

    let (row_count, partition): (i32, String) = sqlx::query_as(
        "SELECT row_count, first_storage_partition_id FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        row_count, 1,
        "the dead-letter row should record one buffered event"
    );
    assert_eq!(
        partition, "poison-partition",
        "the dead-letter row should retain the source partition for triage"
    );

    // Recovery: a new writer over the same journal (with the target restored by
    // its own idempotent schema bootstrap) replays the retained, unacked row and
    // finally persists it -- proving the journal kept it.
    let (tx2, rx2) = mpsc::channel::<LineageEvent>(64);
    let handle2 = spawn_writer(rx2, config, pool.clone()).await?;
    drop(tx2);
    let recovery_stats = handle2.shutdown().await?;
    assert!(
        recovery_stats.written >= 1,
        "the replayed journal row should be written on recovery, got {}",
        recovery_stats.written
    );

    let written_after_restart: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(turn_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        written_after_restart, 1,
        "the retained journal row must be replayed exactly once into turn_lineage"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

/// Builds a minimal but valid retrieval lineage event for one turn.
fn retrieval_event(turn_id: Uuid, storage_partition_id: &str) -> LineageEvent {
    LineageEvent::Retrieval(RetrievalLineage {
        turn_id: moa_lineage_core::TurnId(turn_id),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new(storage_partition_id),
        user_id: UserId::new("writer-db-user"),
        scope: MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::from_u128(0x7)),
        },
        ts: Utc::now(),
        query_original: "what is oauth".to_string(),
        query_expansions: Vec::new(),
        vector_hits: Vec::new(),
        graph_paths: Vec::new(),
        fusion_scores: Vec::new(),
        rerank_scores: Vec::new(),
        top_k: Vec::new(),
        searched_scopes: Vec::new(),
        selected_hits: Vec::new(),
        filters: serde_json::Value::Null,
        timings: StageTimings::default(),
        introspection: BackendIntrospection::default(),
        stage: RetrievalStage::Single,
    })
}

async fn isolated_pool() -> TestResult<(sqlx::PgPool, String, sqlx::PgPool)> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@127.0.0.1:10040/moa".to_string());
    let database_name = format!("lineage_writer_test_{}", Uuid::now_v7().simple());
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
                "isolated lineage writer test database should be reachable: {error}"
            ))
        })?;
    Ok((pool, database_name, cleanup_pool))
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
