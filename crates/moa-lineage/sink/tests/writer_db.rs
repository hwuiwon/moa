//! DB-backed coverage for the lineage writer worker.
//!
//! These tests drive the real writer against an isolated Postgres database
//! carrying the full central migration set. They pin the durability contracts
//! that have no other integration coverage:
//!
//! 1. acceptance means committed - a durable batch is visible to an independent
//!    connection the moment its call returns, so no rollout can lose it;
//! 2. a committed batch a dead replica never finished is completed by another
//!    replica once the lease expires;
//! 3. storing a batch and dequeuing it are one transaction, in both the success
//!    and the permanent-failure direction;
//! 4. a recoverable failure preserves the accepted rows;
//! 5. lineage for a purged tenant cannot be written back after the fence; and
//! 6. a shutdown that runs out of drain budget leaves accepted rows for someone
//!    else rather than consuming them.
//!
//! They require `MOA_DATABASE_URL` to point at a reachable Postgres superuser
//! role that can `CREATE DATABASE`; each test creates and drops its own database.

use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use moa_core::traits::LineageHandle;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_lineage_core::{
    BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings,
};
use moa_lineage_sink::{LineageStore, MpscSink, MpscSinkConfig, WriterState, spawn_writer};
use moa_memory_types::MemoryScope;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::mpsc;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn lineage_writer_flush_on_shutdown_drains_pending_rows_db() -> TestResult<()> {
    let partition = test_partition();
    // Pins: events buffered in the writer's batch (never reaching the size or
    // age flush thresholds) are still drained to Postgres at graceful shutdown.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;

    let config = test_sink_config(Duration::from_secs(3600));

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, LineageStore::new(pool.clone())).await?;

    let turn_ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for turn_id in &turn_ids {
        tx.send(retrieval_event(*turn_id, &partition))
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
    .bind(&partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(written, 3, "three lineage rows should be durably persisted");

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn lineage_writer_poison_batch_dead_letters_and_acks_journal_db() -> TestResult<()> {
    let partition = test_partition();
    // Pins: a batch whose write fails non-retryably is moved once to
    // `analytics.lineage_dead_letters` AND dequeued from the acceptance queue in
    // the same transaction, so a second replica over the SAME queue neither
    // replays the row into `turn_lineage` nor re-dead-letters it. The second
    // writer here is a genuinely independent claimant, not a restart over a
    // private directory only one pod could read.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;

    let config = test_sink_config(Duration::from_secs(3600));

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config.clone(), LineageStore::new(pool.clone())).await?;

    // A check violation is permanent: retrying the identical row cannot make it
    // valid, so the batch belongs in dead-letter storage immediately.
    poison_turn_lineage_with_sqlstate(&pool, "23514").await?;

    let turn_id = Uuid::now_v7();
    tx.send(retrieval_event(turn_id, &partition))
        .await
        .map_err(|error| test_error(format!("send should enqueue event: {error}")))?;
    drop(tx);

    let stats = handle.shutdown().await?;
    assert_eq!(
        stats.written, 0,
        "the poison batch must not count as written"
    );
    assert_eq!(
        stats.pending, 0,
        "the dead-lettered batch must be gone from the acceptance queue, not left \
         for the next claimant to dead-letter again"
    );

    let (dead_letters, row_count, dead_letter_partition): (i64, i32, String) = sqlx::query_as(
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
        dead_letter_partition, partition,
        "the dead-letter row should retain the source partition for triage"
    );

    sqlx::query("DROP TRIGGER poison_lineage ON analytics.turn_lineage")
        .execute(&pool)
        .await?;
    // A second writer over the same queue does not replay the row or create a
    // second dead letter, because the first terminal transaction dequeued it.
    let (tx2, rx2) = mpsc::channel::<LineageEvent>(64);
    let handle2 = spawn_writer(rx2, config, LineageStore::new(pool.clone())).await?;
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
    // Pins: at-least-once dead-lettering yields exactly one row. Processing the
    // identical batch twice (two fresh writer lifetimes over a still-poisoned
    // target) re-derives the same content-addressed dead_letter_id and upserts
    // it, so a crash between the dead-letter commit and anything after it cannot
    // multiply the record.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let turn_id = Uuid::now_v7();
    let partition = test_partition();
    let event = retrieval_event(turn_id, &partition);

    for _ in 0..2 {
        let config = test_sink_config(Duration::from_secs(3600));
        let (tx, rx) = mpsc::channel::<LineageEvent>(64);
        // spawn_writer bootstraps the schema, so drop the target AFTER it opens
        // to keep the write path poisoned for this lifetime.
        let handle = spawn_writer(rx, config, LineageStore::new(pool.clone())).await?;
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
    let config = test_sink_config(Duration::from_secs(3600));

    let (tx, rx) = mpsc::channel::<LineageEvent>(64);
    let handle = spawn_writer(rx, config, LineageStore::new(pool.clone())).await?;

    let partition = test_partition();
    sqlx::query(
        r#"
        INSERT INTO analytics.compliance_tenants
            (storage_partition_id, s3_bucket, signing_key_label, enabled)
        VALUES ($1, 'test-bucket', 'test-signing-key', TRUE)
        "#,
    )
    .bind(&partition)
    .execute(&pool)
    .await?;

    // Distinct, strictly increasing timestamps so read-back `ORDER BY ts` matches the send/chain
    // order deterministically.
    let base = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
        chrono::Utc::now().timestamp_micros(),
    )
    .expect("microsecond timestamp");
    for offset in 0..3_i64 {
        let ts = base + chrono::Duration::seconds(offset);
        tx.send(retrieval_event_at(Uuid::now_v7(), &partition, ts))
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
    .bind(&partition)
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
    .bind(&partition)
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

/// Builds a minimal but valid retrieval lineage event for one turn.
/// Returns a fresh tenant-scoped storage partition.
///
/// The central migration set derives `analytics.turn_lineage.tenant_id` from the
/// partition and refuses a value it cannot parse, so tests must use the same
/// tenant-UUID partitions production does. A made-up label would fail the
/// production trigger, which is the point.
fn test_partition() -> String {
    StoragePartitionId::for_tenant(TenantId::from(Uuid::now_v7())).to_string()
}

fn retrieval_event(turn_id: Uuid, storage_partition_id: &str) -> LineageEvent {
    retrieval_event_for_user(
        turn_id,
        storage_partition_id,
        "writer-db-user",
        chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
    )
}

/// Builds a retrieval lineage event with an explicit timestamp for deterministic chain ordering.
fn retrieval_event_at(
    turn_id: Uuid,
    storage_partition_id: &str,
    ts: chrono::DateTime<Utc>,
) -> LineageEvent {
    retrieval_event_for_user(turn_id, storage_partition_id, "writer-db-user", ts)
}

fn retrieval_event_for_user(
    turn_id: Uuid,
    storage_partition_id: &str,
    user_id: &str,
    ts: chrono::DateTime<Utc>,
) -> LineageEvent {
    LineageEvent::Retrieval(RetrievalLineage {
        turn_id: moa_lineage_core::TurnId(turn_id),
        session_id: SessionId::new(),
        storage_partition_id: StoragePartitionId::new(storage_partition_id),
        user_id: UserId::new(user_id),
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

async fn insert_retrieval_journal_row(pool: &sqlx::PgPool, event: &LineageEvent) -> TestResult<()> {
    let LineageEvent::Retrieval(record) = event else {
        return Err(test_error(
            "journal fixture requires a retrieval event".to_string(),
        ));
    };
    let event_payload = serde_json::to_value(event)?;
    let integrity_hash = moa_lineage_core::chain::canonical_payload_hash(&event_payload)?
        .as_bytes()
        .to_vec();
    let payload = serde_json::json!({
        "table": "lineage",
        "row": {
            "turn_id": record.turn_id.0,
            "session_id": record.session_id.0,
            "user_id": record.user_id.to_string(),
            "storage_partition_id": record.storage_partition_id.to_string(),
            "ts": record.ts,
            "tier": 1,
            "record_kind": event.record_kind().as_i16(),
            "payload": event_payload,
            "integrity_hash": integrity_hash,
            "prev_hash": null,
        }
    });
    sqlx::query(
        "INSERT INTO analytics.lineage_journal \
         (journal_id, storage_partition_id, user_id, event_class, payload) \
         VALUES ($1, $2, $3, 'lineage', $4)",
    )
    .bind(Uuid::now_v7())
    .bind(record.storage_partition_id.as_str())
    .bind(record.user_id.as_str())
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
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
    // The full central migration set, not just the lineage bootstrap. The
    // acceptance queue's row-level security, and the destruction fence the
    // writer refuses to write past, are both defined there. A lineage-only
    // schema would let these tests pass against a permissive stand-in that
    // production never runs.
    for extension in [
        "CREATE EXTENSION IF NOT EXISTS vector",
        "CREATE EXTENSION IF NOT EXISTS pgaudit",
    ] {
        sqlx::raw_sql(extension).execute(&pool).await?;
    }
    let target_url = match database_url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database_name}"),
        None => database_url.clone(),
    };
    moa_migrations::run(&target_url)
        .await
        .map_err(|error| test_error(format!("central migrations should apply: {error}")))?;
    Ok((pool, database_name, cleanup_pool))
}

/// Writer configuration for these tests.
///
/// `batch_max_age` doubles as the drain poll cadence, so a long value keeps a
/// test deterministic (nothing happens until an explicit shutdown) and a short
/// one lets the writer poll on its own.
fn test_sink_config(batch_max_age: Duration) -> MpscSinkConfig {
    MpscSinkConfig {
        channel_capacity: 64,
        batch_size: 100,
        batch_max_age,
        claim_batch_size: 100,
        lease_ttl: Duration::from_secs(60),
        max_pending_age: Duration::from_secs(300),
        drain_timeout: Duration::from_secs(30),
    }
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

#[tokio::test]
async fn lineage_writer_experiment_score_provenance_lands_with_its_score_row_db() -> TestResult<()>
{
    // Pins: the lineage sink is the only writer of Behavior Lab score provenance,
    // and it writes provenance in the SAME transaction as the score row. A score
    // row with no provenance row cannot satisfy a scorecard requirement, so a
    // partial write here would look like complete evidence to nothing and like
    // missing evidence to the gate — both wrong, and neither observable without
    // reading both tables.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let fixture = seed_experiment_fixture(&pool, &database_name).await?;

    let config = test_sink_config(Duration::from_secs(3600));
    let (tx, rx) = mpsc::channel::<LineageEvent>(16);
    let handle = spawn_writer(rx, config, LineageStore::new(pool.clone())).await?;

    let score_id = Uuid::now_v7();
    tx.send(experiment_score_event(
        &fixture,
        score_id,
        "target_completed",
        true,
        replay_stable_ts(),
    ))
    .await
    .map_err(|error| test_error(format!("send should enqueue score: {error}")))?;
    drop(tx);
    let stats = handle.shutdown().await?;
    assert_eq!(stats.written, 1, "the score row must be written");

    let joined: (
        String,
        String,
        String,
        Uuid,
        Uuid,
        Uuid,
        Option<Uuid>,
        Vec<u8>,
    ) = sqlx::query_as(
        "SELECT score.name,
                score.value_type,
                provenance.evaluator_version,
                provenance.experiment_run_uid,
                provenance.plan_revision_uid,
                provenance.trial_uid,
                provenance.target_session_id,
                provenance.evidence_hash
           FROM analytics.scores AS score
           JOIN moa.experiment_score_provenance AS provenance
             ON provenance.score_id = score.score_id
          WHERE score.score_id = $1",
    )
    .bind(score_id)
    .fetch_one(&pool)
    .await?;

    assert_eq!(joined.0, "target_completed");
    assert_eq!(joined.1, "boolean");
    assert_eq!(joined.2, "v1");
    assert_eq!(joined.3, fixture.run_uid);
    assert_eq!(joined.4, fixture.plan_revision_uid);
    assert_eq!(joined.5, fixture.trial_uid);
    assert_eq!(joined.6, Some(fixture.session_id));
    assert_eq!(joined.7, vec![7_u8; 32]);

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn lineage_writer_experiment_score_replay_is_accepted_but_never_rewrites_db() -> TestResult<()>
{
    // Pins the replay contract in both directions. A byte-identical replay of the
    // same score is accepted as a no-op — a workflow retry must not fail. A replay
    // that keeps the score id but changes what the score claims to have observed
    // is REFUSED, because that is a different score wearing the same identity.
    // Before this, the writer's `ON CONFLICT ... DO UPDATE SET <every column>`
    // would have silently rewritten history in exactly that case.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let fixture = seed_experiment_fixture(&pool, &database_name).await?;
    let score_id = Uuid::now_v7();

    // One replay-stable timestamp, as the trial finalizer's journaled
    // `durable_utc_now` step produces. This is what makes a replay land on the
    // same `(score_id, ts)` primary key instead of beside it.
    let ts = replay_stable_ts();

    let identical_replay_stats = {
        let (tx, rx) = mpsc::channel::<LineageEvent>(16);
        let handle = spawn_writer(
            rx,
            test_sink_config(Duration::from_secs(3600)),
            LineageStore::new(pool.clone()),
        )
        .await?;
        for _ in 0..2 {
            tx.send(experiment_score_event(
                &fixture,
                score_id,
                "target_completed",
                true,
                ts,
            ))
            .await
            .map_err(|error| test_error(format!("send should enqueue score: {error}")))?;
        }
        drop(tx);
        handle.shutdown().await?
    };
    assert_eq!(
        identical_replay_stats.written, 2,
        "an identical replay must be accepted rather than refused"
    );

    let rows_after_replay: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.scores WHERE score_id = $1")
            .bind(score_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        rows_after_replay, 1,
        "an identical replay must not add a second score row"
    );

    // Now the same score id derived from different evidence. The provenance
    // comparison must refuse it, and the writer dead-letters the batch rather
    // than absorbing the change.
    let conflicting_stats = {
        let (tx, rx) = mpsc::channel::<LineageEvent>(16);
        let handle = spawn_writer(
            rx,
            test_sink_config(Duration::from_secs(3600)),
            LineageStore::new(pool.clone()),
        )
        .await?;
        let mut event = experiment_score_event(&fixture, score_id, "target_completed", true, ts);
        if let LineageEvent::Eval(record) = &mut event
            && let Some(provenance) = record.experiment_provenance.as_mut()
        {
            provenance.evidence_hash = vec![9_u8; 32];
            provenance.evidence_ref = "session:rewritten#seq=99".to_string();
        }
        tx.send(event)
            .await
            .map_err(|error| test_error(format!("send should enqueue score: {error}")))?;
        drop(tx);
        handle.shutdown().await?
    };
    assert_eq!(
        conflicting_stats.written, 0,
        "a provenance collision must not count as written"
    );

    let stored: (String, Vec<u8>) = sqlx::query_as(
        "SELECT evidence_ref, evidence_hash
           FROM moa.experiment_score_provenance
          WHERE score_id = $1",
    )
    .bind(score_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        stored.0, "session:fixture#seq=1",
        "the refused replay must have left the stored evidence reference untouched"
    );
    assert_eq!(
        stored.1,
        vec![7_u8; 32],
        "the refused replay must have left the stored evidence hash untouched"
    );

    // A NON-replay-stable timestamp is the other half of the contract: the score
    // row is keyed `(score_id, ts)`, so a finalizer that called `Utc::now()` per
    // attempt would insert a SECOND row for the same score. Nothing refuses that
    // at the storage layer, which is exactly why the eligibility gate treats two
    // rows for one requirement as Invalid rather than as a pass.
    let drifted_stats = {
        let (tx, rx) = mpsc::channel::<LineageEvent>(16);
        let handle = spawn_writer(
            rx,
            test_sink_config(Duration::from_secs(3600)),
            LineageStore::new(pool.clone()),
        )
        .await?;
        tx.send(experiment_score_event(
            &fixture,
            score_id,
            "target_completed",
            true,
            ts + chrono::Duration::seconds(1),
        ))
        .await
        .map_err(|error| test_error(format!("send should enqueue score: {error}")))?;
        drop(tx);
        handle.shutdown().await?
    };
    assert_eq!(drifted_stats.written, 1);
    let rows_after_drift: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.scores WHERE score_id = $1")
            .bind(score_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        rows_after_drift, 2,
        "a drifted timestamp duplicates the score row, which the eligibility gate must catch"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

/// Returns a microsecond-truncated timestamp, matching what Postgres stores.
fn replay_stable_ts() -> chrono::DateTime<Utc> {
    chrono::DateTime::<Utc>::from_timestamp_micros(Utc::now().timestamp_micros())
        .expect("microsecond timestamp")
}

/// Identity of the experiment run, trial, and target one fixture score belongs to.
struct ExperimentFixture {
    storage_partition_id: String,
    score_run_id: Uuid,
    run_uid: Uuid,
    trial_uid: Uuid,
    plan_revision_uid: Uuid,
    session_id: Uuid,
}

/// Builds one `LineageEvent::Eval` carrying Behavior Lab provenance.
fn experiment_score_event(
    fixture: &ExperimentFixture,
    score_id: Uuid,
    score_name: &str,
    value: bool,
    ts: chrono::DateTime<Utc>,
) -> LineageEvent {
    LineageEvent::Eval(moa_lineage_core::ScoreRecord {
        score_id,
        ts,
        target: moa_lineage_core::ScoreTarget::Session {
            session_id: SessionId(fixture.session_id),
        },
        storage_partition_id: StoragePartitionId::new(&fixture.storage_partition_id),
        user_id: None,
        name: score_name.to_string(),
        value: moa_lineage_core::ScoreValue::Boolean(value),
        source: moa_lineage_core::ScoreSource::ProductEvaluator,
        model_or_evaluator: "target_completed@v1".to_string(),
        run_id: Some(fixture.score_run_id),
        dataset_id: None,
        comment: None,
        experiment_provenance: Some(moa_lineage_core::ExperimentScoreProvenance {
            experiment_run_uid: fixture.run_uid,
            plan_revision_uid: fixture.plan_revision_uid,
            trial_uid: fixture.trial_uid,
            target: moa_lineage_core::ExperimentScoreTarget::Session {
                session_id: SessionId(fixture.session_id),
            },
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            score_name: score_name.to_string(),
            value_type: "boolean".to_string(),
            evidence_ref: "session:fixture#seq=1".to_string(),
            evidence_hash: vec![7_u8; 32],
        }),
    })
}

/// Applies the full central migration set and seeds the rows V000361's composite
/// foreign keys require.
///
/// `ensure_lineage_schema` alone creates only the `analytics` bootstrap tables,
/// so a provenance write needs the real migration set — which is what production
/// runs, and what makes this test exercise the real constraints rather than a
/// permissive stand-in.
async fn seed_experiment_fixture(
    pool: &sqlx::PgPool,
    database_name: &str,
) -> TestResult<ExperimentFixture> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@127.0.0.1:10040/moa".to_string());
    let target_url = match database_url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{database_name}"),
        None => database_url.clone(),
    };
    let _ = &target_url;

    let tenant_id = Uuid::now_v7();
    let fixture = ExperimentFixture {
        storage_partition_id: StoragePartitionId::for_tenant(TenantId(tenant_id)).to_string(),
        score_run_id: Uuid::now_v7(),
        run_uid: Uuid::now_v7(),
        trial_uid: Uuid::now_v7(),
        plan_revision_uid: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
    };

    sqlx::query(
        "INSERT INTO analytics.score_run (run_id, storage_partition_id, user_id, source)
         VALUES ($1, $2, NULL, 'experiment_trial')",
    )
    .bind(fixture.score_run_id)
    .bind(&fixture.storage_partition_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.experiment_run (
             run_uid, storage_partition_id, user_id, name, target_kind, status, target, variant,
             scorecard, score_run_id, artifact_revision_uids, created_by_identity
         ) VALUES ($1, $2, NULL, 'sink fixture', 'agent_loop', 'running', '{}'::jsonb, '{}'::jsonb,
                   '{}'::jsonb, $3, '{}', '{}'::jsonb)",
    )
    .bind(fixture.run_uid)
    .bind(&fixture.storage_partition_id)
    .bind(fixture.score_run_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO moa.experiment_trial (
             trial_uid, run_uid, storage_partition_id, user_id, trial_key, status, target_kind,
             variant_key, plan_revision_uid, simulator, simulator_model, score_run_id
         ) VALUES ($1, $2, $3, NULL, 'sink/0', 'running', 'agent_loop', 'baseline', $4,
                   '{}'::jsonb, 'sim-model', $5)",
    )
    .bind(fixture.trial_uid)
    .bind(fixture.run_uid)
    .bind(&fixture.storage_partition_id)
    .bind(fixture.plan_revision_uid)
    .bind(fixture.score_run_id)
    .execute(pool)
    .await?;
    Ok(fixture)
}

#[tokio::test]
async fn durable_acceptance_is_committed_before_the_call_returns_db() -> TestResult<()> {
    // Pins THE acceptance boundary. `record_durable_batch` returning must mean
    // the batch is committed in Postgres, observable from a connection that
    // shares nothing with the writer. The shape this replaces returned after an
    // fsync to a pod-local directory, so "accepted" survived exactly as long as
    // the pod did.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();

    // A long poll interval so nothing drains behind our back: the rows we look
    // for must be there because acceptance committed them, not because a drain
    // happened to run.
    let (sink, handle) = MpscSink::spawn(
        test_sink_config(Duration::from_secs(3600)),
        LineageStore::new(pool.clone()),
    )
    .await?;

    let turn_id = Uuid::now_v7();
    let event = retrieval_event(turn_id, &partition);
    LineageHandle::record_durable_batch(&sink, vec![serde_json::to_value(&event)?])
        .await
        .map_err(|error| test_error(format!("durable batch should be accepted: {error}")))?;

    // An independent connection, opened after the call returned.
    let observer = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(PgConnectOptions::from_str(&test_database_url())?.database(&database_name))
        .await?;
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $1",
    )
    .bind(&partition)
    .fetch_one(&observer)
    .await?;
    assert_eq!(
        queued, 1,
        "a returned durable batch must already be committed and visible to an \
         independent connection; found {queued} queued rows for {partition}"
    );
    observer.close().await;

    handle.shutdown().await?;
    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn a_committed_batch_is_finished_by_another_replica_after_a_lease_expires_db()
-> TestResult<()> {
    // Pins the rollout guarantee. A replica accepts a batch, claims it, and dies
    // without storing it. A second replica must finish that exact record with no
    // handoff, no shared filesystem, and nothing noticing the death - only the
    // lease expiring.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();

    // Replica one: accept, then claim under a lease that has already expired by
    // the time we drop it. Dropping the handle aborts its task mid-flight, which
    // is what an evicted pod looks like from the database's side.
    let doomed_config = MpscSinkConfig {
        lease_ttl: Duration::from_millis(1),
        ..test_sink_config(Duration::from_secs(3600))
    };
    let (sink, doomed) = MpscSink::spawn(doomed_config, LineageStore::new(pool.clone())).await?;
    let turn_id = Uuid::now_v7();
    LineageHandle::record_durable_batch(
        &sink,
        vec![serde_json::to_value(retrieval_event(turn_id, &partition))?],
    )
    .await
    .map_err(|error| test_error(format!("durable batch should be accepted: {error}")))?;

    // Lease the row on behalf of the doomed replica, then abandon it.
    sqlx::query(
        "UPDATE analytics.lineage_journal \
         SET lease_owner = gen_random_uuid(), lease_expires_at = now() - interval '1 second' \
         WHERE storage_partition_id = $1",
    )
    .bind(&partition)
    .execute(&pool)
    .await?;
    drop(sink);
    drop(doomed);

    let still_unstored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(turn_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        still_unstored, 0,
        "the precondition of this test is that the dead replica stored nothing; if it \
         already stored the row, the surviving replica below proves nothing"
    );

    // Replica two knows nothing about replica one.
    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let survivor = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(50)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    let stats = survivor.shutdown().await?;

    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(turn_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        stored, 1,
        "the surviving replica must complete the dead replica's committed batch; \
         turn {turn_id} was accepted but never stored"
    );
    assert_eq!(
        stats.pending, 0,
        "and it must dequeue it, leaving nothing for a third claimant"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn an_expired_claimant_cannot_commit_or_dequeue_a_reclaimed_batch_db() -> TestResult<()> {
    // Pins: lease expiry transfers terminal ownership. A stale writer that was
    // already inside the row-store transaction must roll that transaction back
    // when a successor has reclaimed the journal row.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();
    let turn_id = Uuid::now_v7();
    const BLOCK_LOCK: i64 = 8_107_241;

    sqlx::raw_sql(
        r#"
        CREATE OR REPLACE FUNCTION block_lineage_insert_for_lease_test() RETURNS TRIGGER
        LANGUAGE plpgsql AS $block$
        BEGIN
            PERFORM pg_advisory_xact_lock(8107241);
            RETURN NEW;
        END
        $block$;
        CREATE TRIGGER block_lineage_insert_for_lease_test
            BEFORE INSERT ON analytics.turn_lineage
            FOR EACH ROW EXECUTE FUNCTION block_lineage_insert_for_lease_test();
        "#,
    )
    .execute(&pool)
    .await?;
    let mut blocker = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(BLOCK_LOCK)
        .execute(&mut *blocker)
        .await?;

    let first_config = MpscSinkConfig {
        lease_ttl: Duration::from_millis(100),
        drain_timeout: Duration::from_secs(10),
        ..test_sink_config(Duration::from_millis(20))
    };
    let (first_tx, first_rx) = mpsc::channel::<LineageEvent>(8);
    let first = spawn_writer(first_rx, first_config, LineageStore::new(pool.clone())).await?;
    insert_retrieval_journal_row(&pool, &retrieval_event(turn_id, &partition)).await?;
    let first_owner = wait_for_journal_claim(&pool, &partition, 1).await?;

    tokio::time::sleep(Duration::from_millis(150)).await;
    let second_config = MpscSinkConfig {
        lease_ttl: Duration::from_secs(5),
        drain_timeout: Duration::from_secs(10),
        ..test_sink_config(Duration::from_millis(20))
    };
    let (second_tx, second_rx) = mpsc::channel::<LineageEvent>(8);
    let second = spawn_writer(second_rx, second_config, LineageStore::new(pool.clone())).await?;
    let second_owner = wait_for_journal_claim(&pool, &partition, 2).await?;
    assert_ne!(
        second_owner, first_owner,
        "the expired row must be reclaimed by a distinct writer owner"
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(BLOCK_LOCK)
        .execute(&mut *blocker)
        .await?;
    drop(blocker);
    wait_for_journal_empty(&pool, &partition).await?;
    drop(first_tx);
    drop(second_tx);
    let first_stats = first.shutdown().await?;
    let second_stats = second.shutdown().await?;
    assert_eq!(
        (first_stats.written, second_stats.written),
        (0, 1),
        "the stale claimant must roll back; only the reclaiming owner may count the row as written"
    );

    let (stored, queued, dead_letters): (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1), \
           (SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $2), \
           (SELECT count(*) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1)",
    )
    .bind(turn_id)
    .bind(&partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (stored, queued, dead_letters),
        (1, 0, 0),
        "only the current lease owner may commit the row and dequeue its claim"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn a_recoverable_write_failure_preserves_the_accepted_rows_db() -> TestResult<()> {
    // Pins: storing and dequeuing are one transaction. A retryable failure inside
    // the store must roll the dequeue back with it, leaving the accepted row in
    // the queue with its attempt recorded. If the dequeue could commit
    // separately, this is precisely where an accepted record would vanish.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();

    let (sink, handle) = MpscSink::spawn(
        test_sink_config(Duration::from_secs(3600)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    // Poison BEFORE accepting, not after. The durable call wakes the drain, so a
    // batch accepted first can be stored before the trigger lands - which passed
    // in isolation and failed under a loaded parallel run. The writer's schema
    // bootstrap has already happened at `spawn`, so this is safe here.
    //
    // A serialization failure is retryable by SQLSTATE, so the writer must
    // preserve rather than dead-letter.
    poison_turn_lineage_with_sqlstate(&pool, "40001").await?;

    let turn_id = Uuid::now_v7();
    LineageHandle::record_durable_batch(
        &sink,
        vec![serde_json::to_value(retrieval_event(turn_id, &partition))?],
    )
    .await
    .map_err(|error| test_error(format!("durable batch should be accepted: {error}")))?;

    let stats = handle.shutdown().await?;

    assert_eq!(
        stats.written, 0,
        "the failing batch must not count as written"
    );
    let (queued, attempts, leased, deferred): (i64, i32, i64, bool) = sqlx::query_as(
        "SELECT count(*), COALESCE(max(attempts), 0), \
                count(*) FILTER (WHERE lease_owner IS NOT NULL), \
                COALESCE(bool_and(available_at > now()), false) \
           FROM analytics.lineage_journal WHERE storage_partition_id = $1",
    )
    .bind(&partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        queued, 1,
        "a recoverable failure must leave the accepted row queued; found {queued} rows"
    );
    assert_eq!(
        attempts, 1,
        "the failed claim must be recorded as an attempt"
    );
    assert_eq!(
        leased, 0,
        "the lease must be released so any replica can take the next attempt"
    );
    assert!(
        deferred,
        "the row must be deferred into the future so the next attempt backs off"
    );

    let dead_letters: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analytics.lineage_dead_letters WHERE first_turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dead_letters, 0,
        "a transient database failure must never consume the record it could not write"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn a_purged_tenant_cannot_have_lineage_written_back_after_the_fence_db() -> TestResult<()> {
    // Pins the purge race. A batch accepted before a tenant purge is still in the
    // queue when the purge commits its permanent fence. Whatever order the claim,
    // lease, retry and commit happen in, the row must not reappear in
    // `turn_lineage` afterwards - and it must not sit in the queue forever
    // either, because a row nothing will ever store is a permanent backlog.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let purged_tenant = Uuid::now_v7();
    let purged = StoragePartitionId::for_tenant(TenantId::from(purged_tenant)).to_string();
    let neighbour = test_partition();

    let purged_turn = Uuid::now_v7();
    let neighbour_turn = Uuid::now_v7();
    insert_retrieval_journal_row(&pool, &retrieval_event(purged_turn, &purged)).await?;
    insert_retrieval_journal_row(&pool, &retrieval_event(neighbour_turn, &neighbour)).await?;

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $1",
    )
    .bind(&purged)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        queued, 1,
        "precondition: the purged tenant's row must still be queued when the fence lands, \
         or this test is not exercising the accepted-before-purge ordering at all"
    );

    // The purge commits its fence while the batch is still queued.
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
         (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, NULL, 'lineage-race', 'tenant.purge')",
    )
    .bind(purged_tenant)
    .execute(&pool)
    .await?;

    // A second replica, which knows nothing about the purge, drains the queue.
    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let handle = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(50)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    let stats = handle.shutdown().await?;

    let resurrected: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(purged_turn)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        resurrected, 0,
        "lineage accepted before the purge must not be written after it; turn \
         {purged_turn} reappeared for purged tenant {purged_tenant}"
    );
    let survived: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(neighbour_turn)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        survived, 1,
        "the un-purged neighbour in the SAME batch must still be stored; a fence that \
         discarded the whole batch would satisfy the assertion above for the wrong reason"
    );
    assert_eq!(
        stats.pending, 0,
        "the fenced row must be dequeued rather than retried forever"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn a_subject_fence_blocks_only_that_contacts_lineage_while_tenant_fence_blocks_all_db()
-> TestResult<()> {
    // Pins: subject erasure is not tenant erasure. A contact fence must match
    // UUID and `contact:<UUID>` user ids without discarding an unfenced
    // neighbour in the same tenant; a tenant-wide fence still covers both.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let tenant_id = Uuid::now_v7();
    let partition = StoragePartitionId::for_tenant(TenantId::from(tenant_id)).to_string();
    let fenced_contact = Uuid::now_v7();
    let neighbour_contact = Uuid::now_v7();
    let fenced_turn = Uuid::now_v7();
    let neighbour_turn = Uuid::now_v7();

    insert_retrieval_journal_row(
        &pool,
        &retrieval_event_for_user(
            fenced_turn,
            &partition,
            &format!("contact:{fenced_contact}"),
            Utc::now(),
        ),
    )
    .await?;
    insert_retrieval_journal_row(
        &pool,
        &retrieval_event_for_user(
            neighbour_turn,
            &partition,
            &format!("contact:{neighbour_contact}"),
            Utc::now(),
        ),
    )
    .await?;
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
         (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, $2, $3, 'privacy.erase')",
    )
    .bind(tenant_id)
    .bind(fenced_contact)
    .bind(format!("erase-{fenced_contact}"))
    .execute(&pool)
    .await?;

    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let writer = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(20)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    writer.shutdown().await?;

    let stored: Vec<Uuid> = sqlx::query_scalar(
        "SELECT turn_id FROM analytics.turn_lineage \
         WHERE storage_partition_id = $1 ORDER BY turn_id",
    )
    .bind(&partition)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        stored,
        vec![neighbour_turn],
        "the subject fence must discard exactly the fenced contact"
    );

    let tenant_fenced_turn = Uuid::now_v7();
    insert_retrieval_journal_row(
        &pool,
        &retrieval_event_for_user(
            tenant_fenced_turn,
            &partition,
            &format!("contact:{neighbour_contact}"),
            Utc::now(),
        ),
    )
    .await?;
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
         (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, NULL, $2, 'tenant.purge')",
    )
    .bind(tenant_id)
    .bind(format!("purge-{tenant_id}"))
    .execute(&pool)
    .await?;
    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let writer = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(20)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    writer.shutdown().await?;

    let tenant_fenced_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(tenant_fenced_turn)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        tenant_fenced_count, 0,
        "a tenant fence must cover even a subject not named by the earlier contact fence"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn destruction_lock_serializes_a_fence_with_an_in_flight_lineage_write_db() -> TestResult<()>
{
    // Pins: destruction and lineage take the same tenant advisory lock. The
    // writer may claim while destruction is in progress, but cannot pass the
    // fence check until the fence transaction commits.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let tenant_id = Uuid::now_v7();
    let partition = StoragePartitionId::for_tenant(TenantId::from(tenant_id)).to_string();
    let turn_id = Uuid::now_v7();
    insert_retrieval_journal_row(&pool, &retrieval_event(turn_id, &partition)).await?;

    let mut fence_tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(\
         hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
    )
    .bind(tenant_id)
    .execute(&mut *fence_tx)
    .await?;

    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let writer = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(20)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    wait_for_journal_attempts(&pool, &partition, 1).await?;

    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
         (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, NULL, $2, 'tenant.purge')",
    )
    .bind(tenant_id)
    .bind(format!("locked-purge-{tenant_id}"))
    .execute(&mut *fence_tx)
    .await?;
    fence_tx.commit().await?;
    writer.shutdown().await?;

    let (stored, queued): (i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1), \
           (SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $2)",
    )
    .bind(turn_id)
    .bind(&partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (stored, queued),
        (0, 0),
        "the post-lock fence must suppress and dequeue the in-flight row"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn a_drain_that_runs_out_of_budget_leaves_accepted_rows_for_another_replica_db()
-> TestResult<()> {
    // Pins: the outer shutdown deadline aborts even a database operation that
    // never returns. The accepted row stays queued and a successor completes it
    // after the abandoned lease expires.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();
    let turn_id = Uuid::now_v7();
    const BLOCK_LOCK: i64 = 8_107_242;

    sqlx::raw_sql(
        r#"
        CREATE OR REPLACE FUNCTION block_lineage_insert_for_shutdown_test() RETURNS TRIGGER
        LANGUAGE plpgsql AS $block$
        BEGIN
            PERFORM pg_advisory_xact_lock(8107242);
            RETURN NEW;
        END
        $block$;
        CREATE TRIGGER block_lineage_insert_for_shutdown_test
            BEFORE INSERT ON analytics.turn_lineage
            FOR EACH ROW EXECUTE FUNCTION block_lineage_insert_for_shutdown_test();
        "#,
    )
    .execute(&pool)
    .await?;
    let mut blocker = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(BLOCK_LOCK)
        .execute(&mut *blocker)
        .await?;
    let config = MpscSinkConfig {
        lease_ttl: Duration::from_millis(100),
        drain_timeout: Duration::from_millis(50),
        ..test_sink_config(Duration::from_millis(20))
    };
    let (sink, handle) = MpscSink::spawn(config, LineageStore::new(pool.clone())).await?;
    LineageHandle::record_durable_batch(
        &sink,
        vec![serde_json::to_value(retrieval_event(turn_id, &partition))?],
    )
    .await
    .map_err(|error| test_error(format!("durable batch should be accepted: {error}")))?;
    wait_for_journal_attempts(&pool, &partition, 1).await?;

    let started = tokio::time::Instant::now();
    let error = handle
        .shutdown()
        .await
        .expect_err("the blocked store must exhaust the shutdown budget");
    assert!(
        matches!(error, moa_lineage_sink::Error::DrainTimeout { .. }),
        "shutdown must report its bounded drain timeout, got {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a 50ms shutdown budget must not wait indefinitely on Postgres"
    );

    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $1",
    )
    .bind(&partition)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        queued, 1,
        "the aborted drain must preserve its accepted row"
    );

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(BLOCK_LOCK)
        .execute(&mut *blocker)
        .await?;
    drop(blocker);
    sqlx::query("DROP TRIGGER block_lineage_insert_for_shutdown_test ON analytics.turn_lineage")
        .execute(&pool)
        .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (tx, rx) = mpsc::channel::<LineageEvent>(8);
    let survivor = spawn_writer(
        rx,
        test_sink_config(Duration::from_millis(50)),
        LineageStore::new(pool.clone()),
    )
    .await?;
    drop(tx);
    survivor.shutdown().await?;
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM analytics.turn_lineage WHERE turn_id = $1")
            .bind(turn_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        stored, 1,
        "the accepted row must remain completable by another replica"
    );

    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

#[tokio::test]
async fn an_over_age_backlog_fails_readiness_while_the_writer_stays_alive_db() -> TestResult<()> {
    // Pins: an accepted record that is not getting stored makes this replica
    // unready, and it does so from the queue's own view rather than from anything
    // this process remembers. It must NOT report failed - the task is healthy,
    // the backlog is not, and restarting the process would only drop leases.
    let (pool, database_name, cleanup_pool) = isolated_pool().await?;
    let partition = test_partition();

    let config = MpscSinkConfig {
        max_pending_age: Duration::from_secs(1),
        ..test_sink_config(Duration::from_millis(50))
    };
    let (sink, handle) = MpscSink::spawn(config, LineageStore::new(pool.clone())).await?;
    assert_eq!(
        handle.unready_reason(),
        None,
        "a fresh writer with an empty queue must be ready"
    );

    // A row that cannot be stored, backdated past the age limit.
    poison_turn_lineage_with_sqlstate(&pool, "40001").await?;
    LineageHandle::record_durable_batch(
        &sink,
        vec![serde_json::to_value(retrieval_event(
            Uuid::now_v7(),
            &partition,
        ))?],
    )
    .await
    .map_err(|error| test_error(format!("durable batch should be accepted: {error}")))?;
    sqlx::query(
        "UPDATE analytics.lineage_journal SET accepted_at = now() - interval '10 minutes' \
         WHERE storage_partition_id = $1",
    )
    .bind(&partition)
    .execute(&pool)
    .await?;

    let reason = wait_for(Duration::from_secs(10), || handle.unready_reason()).await;
    let reason = reason.ok_or_else(|| {
        test_error(format!(
            "an over-age backlog must fail readiness; health was {:?}",
            handle.health()
        ))
    })?;
    assert!(
        reason.contains("over the"),
        "the reason must name the age limit it exceeded so an operator can act on it, got: {reason}"
    );
    let health = handle.health();
    assert_eq!(
        health.state,
        WriterState::Running,
        "an over-age backlog is a readiness condition, not a dead task; health was {health:?}"
    );
    assert_eq!(
        health.fatal_error, None,
        "no fatal error should be recorded for a healthy task with a slow queue"
    );

    handle.shutdown().await?;
    pool.close().await;
    drop_database(&cleanup_pool, &database_name).await;
    Ok(())
}

/// Makes every `analytics.turn_lineage` insert fail with `sqlstate`.
///
/// A trigger rather than a dropped table, so the SQLSTATE - and therefore the
/// writer's retryable/permanent decision - is chosen by the test rather than
/// inferred from whichever failure a schema change happens to produce.
async fn poison_turn_lineage_with_sqlstate(pool: &sqlx::PgPool, sqlstate: &str) -> TestResult<()> {
    sqlx::raw_sql(&format!(
        r#"
        CREATE OR REPLACE FUNCTION pg_temp_poison_lineage() RETURNS TRIGGER
        LANGUAGE plpgsql AS $poison$
        BEGIN
            RAISE EXCEPTION USING ERRCODE = '{sqlstate}',
                MESSAGE = 'lineage write poisoned by test';
        END
        $poison$;
        DROP TRIGGER IF EXISTS poison_lineage ON analytics.turn_lineage;
        CREATE TRIGGER poison_lineage
            BEFORE INSERT ON analytics.turn_lineage
            FOR EACH ROW EXECUTE FUNCTION pg_temp_poison_lineage();
        "#
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// Polls `probe` until it yields a value or `budget` expires.
async fn wait_for<T>(budget: Duration, probe: impl Fn() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_journal_claim(
    pool: &sqlx::PgPool,
    partition: &str,
    minimum_attempts: i32,
) -> TestResult<Uuid> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let claim: Option<(Uuid, i32)> = sqlx::query_as(
            "SELECT lease_owner, attempts FROM analytics.lineage_journal \
             WHERE storage_partition_id = $1 AND lease_owner IS NOT NULL",
        )
        .bind(partition)
        .fetch_optional(pool)
        .await?;
        if let Some((owner, attempts)) = claim
            && attempts >= minimum_attempts
        {
            return Ok(owner);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(test_error(format!(
                "journal row for {partition} was not claimed {minimum_attempts} time(s)"
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_journal_attempts(
    pool: &sqlx::PgPool,
    partition: &str,
    minimum_attempts: i32,
) -> TestResult<()> {
    wait_for_journal_claim(pool, partition, minimum_attempts)
        .await
        .map(|_| ())
}

async fn wait_for_journal_empty(pool: &sqlx::PgPool, partition: &str) -> TestResult<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM analytics.lineage_journal WHERE storage_partition_id = $1",
        )
        .bind(partition)
        .fetch_one(pool)
        .await?;
        if pending == 0 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(test_error(format!(
                "journal row for {partition} did not reach a terminal outcome"
            )));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Returns the configured test Postgres URL.
fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@127.0.0.1:10040/moa".to_string())
}
