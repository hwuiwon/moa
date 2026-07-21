//! Chaos coverage for the ClickHouse analytics exporter against an isolated,
//! fully-migrated Postgres database and a mock ClickHouse server (the
//! `clickhouse` crate `test-util` feature). Two failure modes are pinned:
//!
//! - ClickHouse down mid-export: the pass errors, the events cursor does not
//!   advance, and a later healthy pass re-exports every row with no loss or skip.
//! - Leader lease: while the export advisory lease is held, a running exporter
//!   cannot lead and issues no ClickHouse traffic; when the lease frees, the
//!   exporter takes over and bootstraps schema.

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clickhouse::Client;
use clickhouse::test::{Mock, handlers, status};
use moa_analytics_export::{AnalyticsExporter, EventRawRow};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Advisory-lock key string the exporter hashes for its single-writer lease
/// (mirrors `AnalyticsExporter`'s private `EXPORT_LEASE_KEY`).
const EXPORT_LEASE_KEY: &str = "clickhouse-analytics-export";

/// Builds an exporter over the isolated pool pointed at the mock ClickHouse with
/// a one-second poll (two-second overlap window).
fn exporter(pool: PgPool, mock: &Mock) -> AnalyticsExporter {
    let client = Client::default().with_url(mock.url());
    AnalyticsExporter::with_client(pool, client, "moa".to_string(), 1, 5000)
}

async fn seed_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, storage_partition_id, user_id, channel, model, status) \
         VALUES ($1, $2, $3, 'chat', 'claude', 'running')",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind("user-1")
    .execute(&mut *tx)
    .await?;
    let default_revision = Uuid::parse_str("00000000-0000-4000-8000-000000000a02")?;
    sqlx::query(
        "INSERT INTO session_agent_context \
             (session_id, storage_partition_id, user_id, agent_definition_ref, agent_revision_uid, \
              policy_hash, display_name, policy_snapshot) \
         VALUES ($1, $2, 'user-1', 'agent://system-default', $3, 'test-hash', 'Test Agent', '{}'::jsonb)",
    )
    .bind(session)
    .bind(tenant.to_string())
    .bind(default_revision)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_event(
    pool: &PgPool,
    tenant: Uuid,
    session: Uuid,
    sequence_num: i64,
    event_type: &str,
    payload: serde_json::Value,
    ts: DateTime<Utc>,
) -> TestResult<()> {
    sqlx::query(
        "INSERT INTO events \
             (id, session_id, storage_partition_id, user_id, tenant_id, sequence_num, event_type, \
              payload, timestamp) \
         VALUES ($1, $2, $3, 'user-1', $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(session)
    .bind(tenant.to_string())
    .bind(tenant)
    .bind(sequence_num)
    .bind(event_type)
    .bind(payload)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seeds a two-turn session (six events) so the events pass has real rows.
async fn seed_two_turn_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    seed_session(pool, tenant, session).await?;
    let base = Utc::now() - ChronoDuration::days(1);
    let tool_a = Uuid::now_v7();
    let tool_b = Uuid::now_v7();
    for (seq, event_type, payload, offset_ms) in [
        (
            1,
            "ToolCall",
            json!({"data": {"tool_id": tool_a, "tool_name": "search"}}),
            0,
        ),
        (
            2,
            "ToolResult",
            json!({"data": {"tool_id": tool_a, "success": true, "duration_ms": 42.0}}),
            50,
        ),
        (
            3,
            "BrainResponse",
            json!({"data": {"model": "claude", "cost_cents": 5}}),
            100,
        ),
        (
            4,
            "ToolCall",
            json!({"data": {"tool_id": tool_b, "tool_name": "fetch"}}),
            1000,
        ),
        (
            5,
            "ToolResult",
            json!({"data": {"tool_id": tool_b, "success": false, "duration_ms": 10.0}}),
            1020,
        ),
        (
            6,
            "BrainResponse",
            json!({"data": {"model": "claude", "cost_cents": 8}}),
            1100,
        ),
    ] {
        insert_event(
            pool,
            tenant,
            session,
            seq,
            event_type,
            payload,
            base + ChronoDuration::milliseconds(offset_ms),
        )
        .await?;
    }
    Ok(())
}

/// Reads the persisted events cursor timestamp, if any.
async fn events_cursor(pool: &PgPool) -> TestResult<Option<DateTime<Utc>>> {
    let row: Option<(DateTime<Utc>,)> = sqlx::query_as(
        "SELECT cursor_ts FROM analytics.clickhouse_export_state WHERE table_name = 'events_raw'",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(cursor_ts,)| cursor_ts))
}

#[tokio::test]
async fn analytics_export_clickhouse_down_preserves_cursor_and_rows_db() -> TestResult<()> {
    // Pins: an export pass whose ClickHouse insert fails leaves the events cursor
    // unadvanced, so a later healthy pass re-reads and re-exports every seeded
    // event — no loss, no skip.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    // ClickHouse down: the single events insert returns 503. `export_events`
    // pulls the six rows in one batch, so exactly one insert request is made and
    // the one failure handler is consumed.
    let down = Mock::new();
    down.add(handlers::failure(status::SERVICE_UNAVAILABLE));
    let down_result = exporter(pool.clone(), &down).export_events().await;
    assert!(
        down_result.is_err(),
        "export must error when ClickHouse is down: {down_result:?}"
    );
    assert!(
        events_cursor(&pool).await?.is_none(),
        "a failed pass must not advance (or create) the events cursor"
    );

    // Recovery: a healthy pass re-exports every event exactly once and advances
    // the cursor.
    let healthy = Mock::new();
    let events_handler = healthy.add(handlers::record::<EventRawRow>());
    let touched = exporter(pool.clone(), &healthy).export_events().await?;
    let rows: Vec<EventRawRow> = events_handler.collect().await;
    let mut seqs: Vec<i64> = rows.iter().map(|row| row.sequence_num).collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4, 5, 6],
        "recovery pass re-exports every seeded event with no loss or skip"
    );
    assert_eq!(touched, vec![session], "the touched session is reported");
    assert!(
        events_cursor(&pool).await?.is_some(),
        "the cursor advances after a healthy pass"
    );

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_leader_lease_is_exclusive_and_frees_db() -> TestResult<()> {
    // Pins: while one holder owns the export advisory lease, a running exporter
    // cannot lead and issues no ClickHouse traffic; once the lease frees, the
    // exporter takes over and bootstraps schema.
    //
    // `acquire_lease`/`release_lease` are private, so the incumbent leader is
    // represented by holding the exporter's exact advisory-lock key on a
    // dedicated session (the same `pg_try_advisory_lock(hashtext(...))` the
    // exporter uses), and the subordinate is driven through the public `run`
    // loop. The observable is the subordinate's first ClickHouse request: absent
    // while the lease is held, a `CREATE DATABASE` once it takes over.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();

    // Incumbent leader holds the advisory lease on a dedicated connection.
    let mut holder = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(EXPORT_LEASE_KEY)
        .fetch_one(&mut *holder)
        .await?;
    assert!(acquired, "the incumbent acquires the export lease");

    // Subordinate exporter: runs, but must idle while the lease is held. Once it
    // takes over it bootstraps schema (CREATE DATABASE + ten CREATE TABLEs = 11
    // requests); handlers cover them with margin, and `non_exhaustive` allows the
    // unused ones (the exporter is cancelled right after the bootstrap is
    // observed). The first request captures the CREATE DATABASE takeover signal.
    let mut mock = Mock::new();
    mock.non_exhaustive();
    let first_request = mock.add(handlers::record_ddl());
    for _ in 0..15 {
        let _ = mock.add(handlers::record_ddl());
    }
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(exporter(pool.clone(), &mock).run(cancel.clone()));

    // One future kept alive across both phases: unresolved while the lease is
    // held (the subordinate made no ClickHouse request), resolving to the first
    // bootstrap statement once the lease frees.
    let request = first_request.query();
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), &mut request)
            .await
            .is_err(),
        "subordinate must not touch ClickHouse while the lease is held"
    );

    // Free the lease; the subordinate must take over.
    sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(EXPORT_LEASE_KEY)
        .execute(&mut *holder)
        .await?;
    drop(holder);

    let first_ddl = tokio::time::timeout(Duration::from_secs(5), &mut request)
        .await
        .expect("subordinate takes the freed lease and issues a ClickHouse request");
    assert!(
        first_ddl.contains("CREATE DATABASE"),
        "the taken-over exporter bootstraps schema first: {first_ddl}"
    );

    cancel.cancel();
    let _ = handle.await;
    pool.close().await;
    Ok(())
}
