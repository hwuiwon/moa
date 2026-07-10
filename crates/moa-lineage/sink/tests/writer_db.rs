//! DB-backed coverage for the lineage writer worker.
//!
//! These tests drive the real `spawn_writer` worker against an isolated Postgres
//! database. They pin two durability contracts that have no other integration
//! coverage:
//! 1. graceful shutdown flushes the in-memory batch so pending rows land in
//!    `analytics.turn_lineage`; and
//! 2. a batch whose write fails non-retryably is persisted once to
//!    `analytics.lineage_dead_letters` and its journal sequence is acknowledged
//!    after the DLQ commit (F16), so the journal drains and a restart neither
//!    replays the row into `turn_lineage` nor re-dead-letters it.
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
use moa_lineage_sink::{LineageStore, MpscSinkConfig, spawn_writer};
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
    };

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, LineageStore::Postgres(pool.clone())).await?;

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
async fn lineage_writer_poison_batch_dead_letters_and_acks_journal_db() -> TestResult<()> {
    // Pins (F16): a batch whose write fails non-retryably is moved once to
    // `analytics.lineage_dead_letters` AND its journal sequence is acknowledged
    // after the DLQ commit, so the journal drains (depth 0) and a fresh writer
    // over the same journal neither replays the row into `turn_lineage` nor
    // re-dead-letters it. This is the inverse of the pre-F16 behavior, where the
    // retained sequence was replayed on every restart.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let journal = tempfile::tempdir()?;
    let journal_path = journal.path().to_path_buf();

    let config = MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        journal_path: journal_path.clone(),
    };

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config.clone(), LineageStore::Postgres(pool.clone())).await?;

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
    // Note: `journal_depth` reflects fjall's approximate_len, which counts the
    // removal tombstone alongside the original insert, so it is not a reliable
    // exact-zero signal. The acknowledgement is instead proven deterministically
    // below: a restart over the same journal does not replay the row (replay reads
    // real keys, not the approximate count).

    let (dead_letters, row_count, partition): (i64, i32, String) = sqlx::query_as(
        "SELECT COUNT(*), MAX(row_count), MAX(first_storage_partition_id) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dead_letters, 1,
        "the poison batch should be persisted exactly once to dead-letter storage"
    );
    assert_eq!(
        row_count, 1,
        "the dead-letter row should record one buffered event"
    );
    assert_eq!(
        partition, "poison-partition",
        "the dead-letter row should retain the source partition for triage"
    );

    // Restart over the same journal with a healthy store (its schema bootstrap
    // restores turn_lineage). Because the dead-lettered sequence was acked, the
    // row is NOT replayed into turn_lineage and NOT re-dead-lettered.
    let (tx2, rx2) = mpsc::channel::<LineageEvent>(64);
    let handle2 = spawn_writer(rx2, config, LineageStore::Postgres(pool.clone())).await?;
    drop(tx2);
    let recovery_stats = handle2.shutdown().await?;
    assert_eq!(
        recovery_stats.written, 0,
        "the acked dead-letter row must not be replayed on restart"
    );

    let written_after_restart: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(turn_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        written_after_restart, 0,
        "the dead-lettered row lives only in the DLQ, never replayed into turn_lineage"
    );

    let dead_letters_after_restart: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dead_letters_after_restart, 1,
        "restart must not create a duplicate dead-letter row"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn lineage_writer_repeated_dead_letter_of_same_batch_is_idempotent_db() -> TestResult<()> {
    // Pins (F16): the crash-between-DLQ-commit-and-ack window is safe. Processing the
    // identical batch twice (two fresh writer lifetimes over a still-poisoned target)
    // re-derives the same content-addressed dead_letter_id and upserts it, so
    // at-least-once dead-lettering yields exactly one DLQ row, never a duplicate.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let turn_id = Uuid::now_v7();
    let event = retrieval_event(turn_id, "idempotent-poison");

    for _ in 0..2 {
        let journal = tempfile::tempdir()?;
        let config = MpscSinkConfig {
            channel_capacity: 64,
            batch_size: 100,
            batch_max_age: Duration::from_secs(3600),
            journal_path: journal.path().to_path_buf(),
        };
        let (tx, rx) = mpsc::channel::<LineageEvent>(64);
        // spawn_writer bootstraps the schema, so drop the target AFTER it opens to
        // keep the write path poisoned for this lifetime.
        let handle = spawn_writer(rx, config, LineageStore::Postgres(pool.clone())).await?;
        sqlx::query("DROP TABLE analytics.turn_lineage CASCADE")
            .execute(&pool)
            .await?;
        tx.send(event.clone())
            .await
            .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
        drop(tx);
        handle.shutdown().await?;
    }

    let dead_letters: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dead_letters, 1,
        "identical re-dead-lettered content must upsert to exactly one DLQ row"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn lineage_writer_compliance_partition_writes_verifiable_hash_chain_db() -> TestResult<()> {
    // Pins: for a compliance-enabled partition the batched hash-chain path writes the same chain a
    // per-row HashChain::link walk would, links each row to the prior tip (genesis first), and
    // advances the partition state tip and record count exactly once per row.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let journal = tempfile::tempdir()?;
    let config = MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        journal_path: journal.path().to_path_buf(),
    };

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, LineageStore::Postgres(pool.clone())).await?;

    let partition = "compliance-partition";
    sqlx::query(
        r#"
        INSERT INTO analytics.compliance_tenants
            (storage_partition_id, s3_bucket, signing_key_label, enabled)
        VALUES ($1, 'test-bucket', 'test-signing-key', TRUE)
        "#,
    )
    .bind(partition)
    .execute(&pool)
    .await?;

    // Distinct, strictly increasing timestamps so read-back `ORDER BY ts` matches the send/chain
    // order deterministically.
    let base = Utc::now();
    for offset in 0..3_i64 {
        let ts = base + chrono::Duration::seconds(offset);
        tx.send(retrieval_event_at(Uuid::now_v7(), partition, ts))
            .await
            .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
    }
    drop(tx);

    let stats = handle.shutdown().await?;
    assert_eq!(
        stats.written, 3,
        "all three compliance rows must be written"
    );

    let rows: Vec<(serde_json::Value, Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
        r#"
        SELECT payload, integrity_hash, prev_hash
        FROM analytics.turn_lineage
        WHERE storage_partition_id = $1
        ORDER BY ts ASC
        "#,
    )
    .bind(partition)
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 3, "three compliance rows should be persisted");

    // The stored integrity hashes must verify as a canonical per-row link chain from genesis.
    let verify_input: Vec<(&serde_json::Value, &[u8])> = rows
        .iter()
        .map(|(payload, integrity_hash, _)| (payload, integrity_hash.as_slice()))
        .collect();
    let final_tip = moa_lineage_core::chain::HashChain::verify(verify_input)
        .expect("stored compliance chain must verify against canonical payloads");

    // Each row links to the prior tip; the first links to genesis.
    let genesis = moa_lineage_core::chain::genesis_hash();
    assert_eq!(rows[0].2.as_deref(), Some(genesis.as_bytes().as_slice()));
    assert_eq!(rows[1].2.as_deref(), Some(rows[0].1.as_slice()));
    assert_eq!(rows[2].2.as_deref(), Some(rows[1].1.as_slice()));

    // The partition state tip and count are advanced once per new row by the single batched update.
    let (state_hash, record_count): (Option<Vec<u8>>, i64) = sqlx::query_as(
        r#"
        SELECT last_integrity_hash, record_count
        FROM analytics.compliance_storage_partition_state
        WHERE storage_partition_id = $1
        "#,
    )
    .bind(partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        state_hash.as_deref(),
        Some(final_tip.as_bytes().as_slice()),
        "partition state tip must equal the chain's final integrity hash"
    );
    assert_eq!(record_count, 3, "record count advances once per new row");

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

/// Row shape the mock ClickHouse server decodes from the writer's RowBinary
/// insert; field order and serde attributes mirror the sink's wire row.
#[derive(Debug, serde::Deserialize)]
struct RecordedClickHouseRow {
    #[serde(with = "clickhouse::serde::uuid")]
    turn_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    _session_id: Uuid,
    _user_id: String,
    storage_partition_id: String,
    #[serde(with = "clickhouse::serde::chrono::datetime64::micros")]
    _ts: chrono::DateTime<Utc>,
    _tier: i16,
    record_kind: i16,
    payload: String,
    _answer_text: Option<String>,
    integrity_hash: String,
    _prev_hash: Option<String>,
}

#[tokio::test]
async fn lineage_writer_clickhouse_backend_splits_rows_from_scores_db() -> TestResult<()> {
    // Pins: with [clickhouse] configured, turn_lineage rows land in ClickHouse
    // (bootstrapped with database + TTL table DDL) and NOT in Postgres, while
    // score rows still land in Postgres analytics.scores.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let journal = tempfile::tempdir()?;

    let mock = clickhouse::test::Mock::new();
    let create_database = mock.add(clickhouse::test::handlers::record_ddl());
    let create_table = mock.add(clickhouse::test::handlers::record_ddl());
    let insert = mock.add(clickhouse::test::handlers::record::<RecordedClickHouseRow>());

    let clickhouse_config = moa_core::ClickHouseConfig {
        url: mock.url().to_string(),
        ..moa_core::ClickHouseConfig::default()
    };
    let store = LineageStore::from_config(Some(&clickhouse_config), pool.clone());

    let config = MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age: Duration::from_secs(3600),
        journal_path: journal.path().to_path_buf(),
    };
    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, store).await?;

    let turn_id = Uuid::now_v7();
    let score_id = Uuid::now_v7();
    tx.send(retrieval_event(turn_id, "clickhouse-partition"))
        .await
        .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
    tx.send(LineageEvent::Eval(moa_lineage_core::ScoreRecord {
        score_id,
        ts: Utc::now(),
        target: moa_lineage_core::ScoreTarget::Turn {
            turn_id: moa_lineage_core::TurnId(turn_id),
        },
        storage_partition_id: StoragePartitionId::new("clickhouse-partition"),
        user_id: None,
        name: "retrieval_zero_recall".to_string(),
        value: moa_lineage_core::ScoreValue::Boolean(false),
        source: moa_lineage_core::ScoreSource::OnlineJudge,
        model_or_evaluator: "retriever".to_string(),
        run_id: None,
        dataset_id: None,
        comment: None,
    }))
    .await
    .map_err(|error| test_error(format!("send should enqueue score: {error}")))?;
    drop(tx);

    let stats = handle.shutdown().await?;
    assert_eq!(stats.written, 2, "both rows must count as written");

    let database_ddl = create_database.query().await;
    assert!(
        database_ddl.contains("CREATE DATABASE IF NOT EXISTS"),
        "first DDL must create the database: {database_ddl}"
    );
    let table_ddl = create_table.query().await;
    assert!(
        table_ddl.contains("turn_lineage") && table_ddl.contains("TTL"),
        "second DDL must create the TTL'd turn_lineage table: {table_ddl}"
    );

    let rows: Vec<RecordedClickHouseRow> = insert.collect().await;
    assert_eq!(rows.len(), 1, "exactly the lineage row goes to ClickHouse");
    assert_eq!(rows[0].turn_id, turn_id);
    assert_eq!(rows[0].storage_partition_id, "clickhouse-partition");
    assert_eq!(rows[0].record_kind, 1, "retrieval record kind");
    assert!(
        rows[0].payload.contains("what is oauth"),
        "payload must carry the serialized event JSON"
    );
    assert!(
        !rows[0].integrity_hash.is_empty(),
        "per-row canonical hash must survive the backend switch"
    );

    let postgres_lineage_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analytics.turn_lineage")
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        postgres_lineage_rows, 0,
        "turn_lineage rows must not land in Postgres under the clickhouse backend"
    );
    let postgres_score_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM analytics.scores WHERE score_id = $1")
            .bind(score_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        postgres_score_rows, 1,
        "score rows stay in Postgres under the clickhouse backend"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

/// Builds a minimal but valid retrieval lineage event for one turn.
fn retrieval_event(turn_id: Uuid, storage_partition_id: &str) -> LineageEvent {
    retrieval_event_at(turn_id, storage_partition_id, Utc::now())
}

/// Builds a retrieval lineage event with an explicit timestamp for deterministic chain ordering.
fn retrieval_event_at(
    turn_id: Uuid,
    storage_partition_id: &str,
    ts: chrono::DateTime<Utc>,
) -> LineageEvent {
    LineageEvent::Retrieval(RetrievalLineage {
        turn_id: moa_lineage_core::TurnId(turn_id),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new(storage_partition_id),
        user_id: UserId::new("writer-db-user"),
        scope: MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::from_u128(0x7)),
        },
        ts,
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
