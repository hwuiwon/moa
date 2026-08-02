//! Synthetic load check for the ClickHouse analytics exporter.
//!
//! Seeds a sweep-sized corpus (~200 sessions × ~40 events) with batched
//! `UNNEST` inserts into an isolated, fully-migrated Postgres database, runs one
//! full export pass against a mock ClickHouse, and asserts the pass completes
//! well under a generous wall-clock bound while reporting the actual duration.
//! It also `EXPLAIN`s the exporter's two hottest Postgres pulls (the
//! `dim_sessions` pull and the `events_raw` pull, SQL shapes mirrored from
//! `crates/moa-orchestrator/src/analytics_export/{dims,events}.rs`) with a
//! selective steady-state cursor bound and asserts the driving table is read by
//! index, never sequentially (schema contract rule #4).
//!
//! Gated and ignored (slow). Run with Postgres up:
//! `MOA_RUN_ANALYTICS_LOAD_TESTS=1 MOA_DATABASE_URL=... cargo nextest run \
//!  -p moa-orchestrator --run-ignored all -E 'test(analytics_export_load)'`.

use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clickhouse::Client;
use clickhouse::test::{Mock, handlers};
use moa_analytics_export::AnalyticsExporter;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SESSION_COUNT: usize = 200;
const EVENTS_PER_SESSION: usize = 40;
/// Generous wall-clock ceiling for one full export pass over the corpus.
const PASS_BUDGET: Duration = Duration::from_secs(60);
/// Batch size large enough that each table exports in a single mock insert.
const EXPORT_BATCH_ROWS: usize = 50_000;

/// `dim_sessions` incremental pull, mirrored from `analytics_export/dims.rs`
/// (`export_dim_sessions`). `$1`/`$2` are the `(updated_at, id)` cursor bound,
/// `$3` the batch size; the driving `sessions` scan must use `idx_sessions_updated`.
const DIM_SESSIONS_PULL_SQL: &str = "SELECT s.id AS session_id, s.tenant_id, s.storage_partition_id, s.user_id, \
        s.contact_id, s.status, COALESCE(s.channel, 'chat') AS channel, s.model, s.title, \
        s.parent_session_id, \
        COALESCE(s.total_input_tokens_uncached, 0)::BIGINT AS total_input_tokens_uncached, \
        COALESCE(s.total_input_tokens_cache_write, 0)::BIGINT AS total_input_tokens_cache_write, \
        COALESCE(s.total_input_tokens_cache_read, 0)::BIGINT AS total_input_tokens_cache_read, \
        COALESCE(s.total_output_tokens, 0)::BIGINT AS total_output_tokens, \
        COALESCE(s.total_cost_cents, 0)::BIGINT AS total_cost_cents, \
        COALESCE(s.event_count, 0)::BIGINT AS event_count, \
        COALESCE(s.turn_count, 0)::BIGINT AS turn_count, \
        s.created_at, s.updated_at, s.completed_at, s.updated_at AS export_version \
     FROM sessions s \
     WHERE (s.updated_at > $1 OR (s.updated_at = $1 AND ($2::uuid IS NULL OR s.id > $2))) \
     ORDER BY s.updated_at, s.id LIMIT $3";

/// `events_raw` incremental pull, mirrored from `analytics_export/events.rs`
/// (`EVENTS_SQL`). `$1`/`$2` are the `(timestamp, id)` cursor bound, `$3` the
/// batch size; the driving `events` scan must use `idx_events_timestamp`.
const EVENTS_PULL_SQL: &str = "SELECT e.id AS event_id, e.session_id, s.tenant_id, \
        e.storage_partition_id, e.user_id, e.sequence_num, \
        e.turn_number, \
        e.event_type, e.token_count, e.payload::text AS payload, e.timestamp AS ts \
     FROM events e \
     JOIN sessions s ON s.id = e.session_id \
     WHERE (e.timestamp > $1 OR (e.timestamp = $1 AND ($2::uuid IS NULL OR e.id > $2))) \
     ORDER BY e.timestamp, e.id LIMIT $3";

#[tokio::test]
#[ignore = "slow synthetic load; requires MOA_RUN_ANALYTICS_LOAD_TESTS=1"]
async fn analytics_export_full_pass_load_db() -> TestResult<()> {
    if std::env::var("MOA_RUN_ANALYTICS_LOAD_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_ANALYTICS_LOAD_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let now = moa_test_support::fixtures::pg_now();

    let seed_start = Instant::now();
    seed_sessions(&pool, tenant, now).await?;
    let event_total = seed_events(&pool, tenant, now).await?;
    seed_completed_execution_upgrade_state(&pool, now).await?;
    println!(
        "seeded {SESSION_COUNT} sessions and {event_total} events in {:?}",
        seed_start.elapsed()
    );

    // Mock ClickHouse that returns 200 for every insert; the controls are
    // dropped since only timing and completion matter. Schema/bootstrap
    // correctness belongs to the real ClickHouse lane, while this test isolates
    // the steady-state Postgres pull and transform cost.
    let mut mock = Mock::new();
    mock.non_exhaustive();
    for _ in 0..64 {
        let _ = mock.add(handlers::record_ddl());
    }
    let client = Client::default().with_url(mock.url());
    let exporter = AnalyticsExporter::with_client(
        pool.clone(),
        client,
        "moa".to_string(),
        1,
        EXPORT_BATCH_ROWS,
    );

    let pass_start = Instant::now();
    exporter.run_one_pass().await?;
    let pass_elapsed = pass_start.elapsed();
    println!(
        "full export pass ({SESSION_COUNT} sessions, {event_total} events) completed in {pass_elapsed:?}"
    );
    assert!(
        pass_elapsed < PASS_BUDGET,
        "export pass took {pass_elapsed:?}, over the {PASS_BUDGET:?} budget"
    );

    // Steady-state incremental pulls use a selective cursor bound, so both hot
    // Postgres reads must be index scans on their driving table, never seq scans.
    let sessions_bound = now - ChronoDuration::minutes(5);
    let sessions_plan = explain(&pool, DIM_SESSIONS_PULL_SQL, sessions_bound).await?;
    assert_plan_indexed(&sessions_plan, "sessions", "idx_sessions_updated");

    // `events` is hash-partitioned, so the driving scan is a Merge Append of each
    // partition's timestamp index (`events_pNN_timestamp_idx`, all sharing the
    // `_timestamp_idx` suffix from the partitioned `idx_events_timestamp`).
    let events_bound = now - ChronoDuration::minutes(2);
    let events_plan = explain(&pool, EVENTS_PULL_SQL, events_bound).await?;
    assert_plan_indexed(&events_plan, "events", "_timestamp_idx");

    pool.close().await;
    Ok(())
}

/// Seeds the completed execution-dimension bootstrap required before normal
/// steady-state passes may run. The load corpus contains no execution rows.
async fn seed_completed_execution_upgrade_state(
    pool: &PgPool,
    export_version_floor: DateTime<Utc>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO analytics.clickhouse_schema_upgrade_state ( \
             upgrade_key, database_uuid, run_table_uuid, task_table_uuid, \
             stage, upgrade_version, export_version_floor, \
             run_high_water_seq, run_high_water_id, task_high_water_seq, task_high_water_id, \
             run_page_seq, run_page_id, task_page_seq, task_page_id, completed_at \
         ) VALUES ( \
             'execution_dimensions', $3, $4, $5, 'complete', $1, $1, \
             0, $2, 0, $2, 0, $2, 0, $2, NOW() \
         )",
    )
    .bind(export_version_floor)
    .bind(Uuid::nil())
    .bind(Uuid::from_u128(1))
    .bind(Uuid::from_u128(2))
    .bind(Uuid::from_u128(3))
    .execute(pool)
    .await?;
    for table in ["dim_execution_runs", "dim_execution_tasks"] {
        sqlx::query(
            "INSERT INTO analytics.clickhouse_export_state ( \
                 table_name, cursor_ts, cursor_id, exported_at, cursor_seq \
             ) VALUES ($1, $2, $3, $2, 0)",
        )
        .bind(table)
        .bind(DateTime::<Utc>::UNIX_EPOCH)
        .bind(Uuid::nil())
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Batch-inserts `SESSION_COUNT` sessions with their agent-context rows in one
/// transaction (the agent-context row is required by the deferred session
/// constraint), spreading `updated_at` over a range so the cursor bound is
/// selective.
async fn seed_sessions(pool: &PgPool, tenant: Uuid, now: DateTime<Utc>) -> TestResult<()> {
    let mut ids = Vec::with_capacity(SESSION_COUNT);
    let mut updated = Vec::with_capacity(SESSION_COUNT);
    for index in 0..SESSION_COUNT {
        ids.push(Uuid::now_v7());
        // Oldest session ~200 minutes ago; newest ~1 minute ago.
        updated.push(now - ChronoDuration::minutes((SESSION_COUNT - index) as i64));
    }
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;

    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions \
             (id, storage_partition_id, user_id, channel, model, status, updated_at, created_at, \
              event_count, turn_count) \
         SELECT s.id, $2, 'user-1', 'chat', 'claude', 'completed', s.ts, s.ts, $4, $5 \
         FROM UNNEST($1::uuid[], $3::timestamptz[]) AS s(id, ts)",
    )
    .bind(&ids)
    .bind(tenant.to_string())
    .bind(&updated)
    .bind(EVENTS_PER_SESSION as i64)
    .bind((EVENTS_PER_SESSION / 4) as i64)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO session_agent_context \
             (session_id, storage_partition_id, user_id, agent_definition_ref, agent_revision_uid, \
              policy_hash, display_name, policy_snapshot) \
         SELECT id, $2, 'user-1', 'agent://system-default', $3, 'test-hash', 'Test Agent', '{}'::jsonb \
         FROM UNNEST($1::uuid[]) AS s(id)",
    )
    .bind(&ids)
    .bind(tenant.to_string())
    .bind(default_revision)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Batch-inserts `EVENTS_PER_SESSION` events per session via one `UNNEST`
/// statement, cycling event types (ToolCall/ToolResult/BrainResponse/Error) so a
/// realistic turn structure and payload shape drive the fact recompute. Returns
/// the total event count.
async fn seed_events(pool: &PgPool, tenant: Uuid, now: DateTime<Utc>) -> TestResult<usize> {
    let session_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM sessions ORDER BY updated_at")
        .fetch_all(pool)
        .await?;

    let total = session_ids.len() * EVENTS_PER_SESSION;
    let mut event_ids = Vec::with_capacity(total);
    let mut owner_ids = Vec::with_capacity(total);
    let mut sequence_nums = Vec::with_capacity(total);
    let mut turn_numbers = Vec::with_capacity(total);
    let mut event_types = Vec::with_capacity(total);
    let mut timestamps = Vec::with_capacity(total);

    for (session_index, session_id) in session_ids.iter().enumerate() {
        // Session `session_index` is the `session_index`-th oldest (ORDER BY
        // updated_at), so its events land near its updated_at.
        let session_base = now - ChronoDuration::minutes((SESSION_COUNT - session_index) as i64);
        for event_index in 0..EVENTS_PER_SESSION {
            event_ids.push(Uuid::now_v7());
            owner_ids.push(*session_id);
            sequence_nums.push((event_index + 1) as i64);
            turn_numbers.push(((event_index + 1) / 4 + 1) as i64);
            event_types.push(event_type_for(event_index).to_string());
            timestamps.push(session_base + ChronoDuration::seconds(event_index as i64));
        }
    }

    // Chunk the UNNEST insert so array parameters stay a reasonable size.
    let chunk = 4_000;
    let mut offset = 0;
    while offset < total {
        let end = (offset + chunk).min(total);
        sqlx::query(
            "INSERT INTO events \
                 (id, session_id, storage_partition_id, user_id, tenant_id, sequence_num, \
                  turn_number, event_type, payload, timestamp) \
             SELECT e.id, e.session_id, $7, 'user-1', $8, e.seq, e.turn_number, e.etype, \
                 CASE e.etype \
                     WHEN 'BrainResponse' THEN '{\"data\":{\"model\":\"claude\",\"cost_cents\":5,\"input_tokens_uncached\":10,\"input_tokens_cache_write\":1,\"input_tokens_cache_read\":2,\"output_tokens\":4}}'::jsonb \
                     WHEN 'ToolCall' THEN '{\"data\":{\"tool_id\":\"00000000-0000-4000-8000-000000000001\",\"tool_name\":\"search\"}}'::jsonb \
                     WHEN 'ToolResult' THEN '{\"data\":{\"tool_id\":\"00000000-0000-4000-8000-000000000001\",\"success\":true,\"duration_ms\":10.0}}'::jsonb \
                     ELSE '{\"data\":{}}'::jsonb \
                 END, \
                 e.ts \
             FROM UNNEST($1::uuid[], $2::uuid[], $3::bigint[], $4::bigint[], $5::text[], $6::timestamptz[]) \
                 AS e(id, session_id, seq, turn_number, etype, ts)",
        )
        .bind(&event_ids[offset..end])
        .bind(&owner_ids[offset..end])
        .bind(&sequence_nums[offset..end])
        .bind(&turn_numbers[offset..end])
        .bind(&event_types[offset..end])
        .bind(&timestamps[offset..end])
        .bind(tenant.to_string())
        .bind(tenant)
        .execute(pool)
        .await?;
        offset = end;
    }

    Ok(total)
}

/// Cycles event types so each session has interleaved tool calls, results,
/// brain responses, and errors.
fn event_type_for(event_index: usize) -> &'static str {
    match event_index % 4 {
        0 => "ToolCall",
        1 => "ToolResult",
        2 => "BrainResponse",
        _ => "Error",
    }
}

/// Runs `EXPLAIN` on a pull SQL with a selective `$1` timestamp bound, a NULL
/// `$2` id bound, and a batch `$3` limit, returning the joined plan text.
///
/// A ~200-row corpus is small enough that Postgres correctly prefers a plain
/// sequential scan (the whole table is a page or two), so `enable_seqscan` is
/// disabled for the plan: the assertion then verifies the cursor pull *can* be
/// served by an index range read on the driving table (schema contract rule #4),
/// which is the size-independent property that matters at production scale.
async fn explain(pool: &PgPool, sql: &str, bound: DateTime<Utc>) -> TestResult<String> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await?;
    let lines: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {sql}"))
        .bind(bound)
        .bind(None::<Uuid>)
        .bind(EXPORT_BATCH_ROWS as i64)
        .fetch_all(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(lines.join("\n"))
}

/// Asserts the plan reads `driving_table` via an index (not sequentially).
fn assert_plan_indexed(plan: &str, driving_table: &str, expected_index: &str) {
    let seq_scan = format!("Seq Scan on {driving_table}");
    assert!(
        !plan.contains(&seq_scan),
        "cursor pull must not sequentially scan {driving_table}:\n{plan}"
    );
    assert!(
        plan.contains("Index") && plan.contains(expected_index),
        "cursor pull must use {expected_index} on {driving_table}:\n{plan}"
    );
}
