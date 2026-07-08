//! DB-backed coverage for the ClickHouse analytics exporter.
//!
//! These tests drive the real exporter export methods against an isolated,
//! fully-migrated Postgres database (via `bootstrap_test_db`) and a mock
//! ClickHouse server (the `clickhouse` crate `test-util` feature). They pin the
//! contracts that have no other integration coverage:
//! - `turn_fact` rows equal the `session_turn_metrics` matview field-for-field
//!   (parity by construction, since the SQL is shared);
//! - `events_raw` `turn_number` is stamped as the session's BrainResponse prefix
//!   count;
//! - a dimension upsert supersedes a mutated row with a higher `export_version`;
//! - the events cursor resumes from the persisted position after a restart.

use chrono::{DateTime, Duration, Utc};
use clickhouse::Client;
use clickhouse::test::{Mock, handlers};
use moa_orchestrator::analytics_export::{
    AnalyticsExporter, DimSessionRow, EventRawRow, ToolCallFactRow, TurnFactRow,
};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Builds an exporter over the isolated pool pointed at the mock ClickHouse.
/// A one-second poll gives a two-second overlap window for the cursor test.
fn exporter(pool: PgPool, mock: &Mock) -> AnalyticsExporter {
    exporter_with_batch(pool, mock, 5000)
}

/// Exporter with an explicit batch size, to force multi-batch pulls.
fn exporter_with_batch(pool: PgPool, mock: &Mock, batch_rows: usize) -> AnalyticsExporter {
    let client = Client::default().with_url(mock.url());
    AnalyticsExporter::with_client(pool, client, "moa".to_string(), 1, batch_rows)
}

async fn seed_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    // The `session_requires_agent_context` constraint trigger is deferred to
    // commit, so the session and its agent-context row must land in one
    // transaction.
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
    // The system-default artifact revision seeded by V000306; satisfies the
    // agent_revision_uid FK to moa.artifact_revision.
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

/// Seeds a two-turn session: for each turn a ToolCall, its ToolResult, then a
/// BrainResponse carrying token/cost/model data.
async fn seed_two_turn_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    seed_session(pool, tenant, session).await?;
    let base = Utc::now() - Duration::days(1);
    let tool_a = Uuid::now_v7();
    let tool_b = Uuid::now_v7();

    insert_event(
        pool,
        tenant,
        session,
        1,
        "ToolCall",
        json!({"data": {"tool_id": tool_a, "tool_name": "search"}}),
        base,
    )
    .await?;
    insert_event(
        pool,
        tenant,
        session,
        2,
        "ToolResult",
        json!({"data": {"tool_id": tool_a, "success": true, "duration_ms": 42.0}}),
        base + Duration::milliseconds(50),
    )
    .await?;
    insert_event(
        pool,
        tenant,
        session,
        3,
        "BrainResponse",
        json!({"data": {"model": "claude", "duration_ms": 100.0, "input_tokens_uncached": 10,
            "input_tokens_cache_write": 2, "input_tokens_cache_read": 3, "output_tokens": 7,
            "cost_cents": 5}}),
        base + Duration::milliseconds(100),
    )
    .await?;
    insert_event(
        pool,
        tenant,
        session,
        4,
        "ToolCall",
        json!({"data": {"tool_id": tool_b, "tool_name": "fetch"}}),
        base + Duration::seconds(1),
    )
    .await?;
    insert_event(
        pool,
        tenant,
        session,
        5,
        "ToolResult",
        json!({"data": {"tool_id": tool_b, "success": false, "duration_ms": 10.0}}),
        base + Duration::seconds(1) + Duration::milliseconds(20),
    )
    .await?;
    insert_event(
        pool,
        tenant,
        session,
        6,
        "BrainResponse",
        json!({"data": {"model": "claude", "duration_ms": 200.0, "input_tokens_uncached": 20,
            "input_tokens_cache_write": 0, "input_tokens_cache_read": 1, "output_tokens": 9,
            "cost_cents": 8}}),
        base + Duration::seconds(1) + Duration::milliseconds(100),
    )
    .await?;
    Ok(())
}

/// Matview projection used to diff `turn_fact` row-for-row.
#[derive(Debug, sqlx::FromRow)]
struct MatviewTurn {
    turn_number: i64,
    model: Option<String>,
    llm_ms: f64,
    tool_ms: f64,
    tool_call_count: i64,
    input_tokens_uncached: i64,
    input_tokens_cache_write: i64,
    input_tokens_cache_read: i64,
    total_input_tokens: i64,
    output_tokens: i64,
    cost_cents: i64,
}

#[tokio::test]
async fn analytics_export_turn_fact_matches_matview_db() -> TestResult<()> {
    // Pins: exported turn_fact rows equal the session_turn_metrics matview
    // field-for-field on real event data (shared SQL, validates export plumbing).
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    let events_handler = mock.add(handlers::record::<EventRawRow>());
    let turn_handler = mock.add(handlers::record::<TurnFactRow>());
    let tool_handler = mock.add(handlers::record::<ToolCallFactRow>());
    let exporter = exporter(pool.clone(), &mock);

    let touched = exporter.export_events().await?;
    exporter.export_facts(&touched).await?;

    let _events: Vec<EventRawRow> = events_handler.collect().await;
    let mut turn_rows: Vec<TurnFactRow> = turn_handler.collect().await;
    let _tool_rows: Vec<ToolCallFactRow> = tool_handler.collect().await;
    turn_rows.sort_by_key(|row| row.turn_number);

    sqlx::query("REFRESH MATERIALIZED VIEW session_turn_metrics")
        .execute(&pool)
        .await?;
    let matview: Vec<MatviewTurn> = sqlx::query_as(
        "SELECT turn_number, model, llm_ms, tool_ms, tool_call_count, input_tokens_uncached, \
             input_tokens_cache_write, input_tokens_cache_read, total_input_tokens, output_tokens, \
             cost_cents \
         FROM session_turn_metrics WHERE session_id = $1 ORDER BY turn_number",
    )
    .bind(session)
    .fetch_all(&pool)
    .await?;

    assert_eq!(
        turn_rows.len(),
        matview.len(),
        "turn_fact row count must match matview"
    );
    assert_eq!(turn_rows.len(), 2, "two BrainResponse turns expected");
    for (exported, expected) in turn_rows.iter().zip(matview.iter()) {
        assert_eq!(
            exported.tenant_id, tenant,
            "tenant_id stamped from the joined session"
        );
        assert_eq!(exported.session_id, session);
        assert_eq!(exported.turn_number, expected.turn_number);
        assert_eq!(exported.model, expected.model);
        assert!(
            (exported.llm_ms - expected.llm_ms).abs() < 1e-9,
            "llm_ms parity"
        );
        assert!(
            (exported.tool_ms - expected.tool_ms).abs() < 1e-9,
            "tool_ms parity"
        );
        assert_eq!(exported.tool_call_count, expected.tool_call_count);
        assert_eq!(
            exported.input_tokens_uncached,
            expected.input_tokens_uncached
        );
        assert_eq!(
            exported.input_tokens_cache_write,
            expected.input_tokens_cache_write
        );
        assert_eq!(
            exported.input_tokens_cache_read,
            expected.input_tokens_cache_read
        );
        assert_eq!(exported.total_input_tokens, expected.total_input_tokens);
        assert_eq!(exported.output_tokens, expected.output_tokens);
        assert_eq!(exported.cost_cents, expected.cost_cents);
    }
    // Turn 1's ToolResult reports 42ms; turn 2's reports 10ms — proves the
    // per-turn tool window and duration fallback carried through.
    assert!((turn_rows[0].tool_ms - 42.0).abs() < 1e-9);
    assert!((turn_rows[1].tool_ms - 10.0).abs() < 1e-9);

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_events_stamp_turn_number_db() -> TestResult<()> {
    // Pins: each events_raw row is stamped with 1 + count of prior BrainResponse
    // events in its session; a BrainResponse counts itself.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    let events_handler = mock.add(handlers::record::<EventRawRow>());
    let exporter = exporter(pool.clone(), &mock);

    exporter.export_events().await?;

    let mut rows: Vec<EventRawRow> = events_handler.collect().await;
    rows.sort_by_key(|row| row.sequence_num);
    let turn_numbers: Vec<i64> = rows.iter().map(|row| row.turn_number).collect();
    assert_eq!(
        turn_numbers,
        vec![1, 1, 1, 2, 2, 2],
        "events before/at the first BrainResponse are turn 1; after it, turn 2"
    );
    assert!(rows.iter().all(|row| row.tenant_id == tenant));

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_turn_number_spans_batch_boundary_db() -> TestResult<()> {
    // Pins: turn_number counts BrainResponses over the session's entire prefix,
    // not just the current export batch. With batch_rows=2 the six events are
    // pulled in three batches; the second turn's events land in a later batch
    // than the BrainResponse that opened turn 1, yet are still stamped turn 2.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    // Three batches of two rows each -> three inserts.
    let batch_handlers = [
        mock.add(handlers::record::<EventRawRow>()),
        mock.add(handlers::record::<EventRawRow>()),
        mock.add(handlers::record::<EventRawRow>()),
    ];
    let exporter = exporter_with_batch(pool.clone(), &mock, 2);

    exporter.export_events().await?;

    let mut rows: Vec<EventRawRow> = Vec::new();
    for handler in batch_handlers {
        rows.extend(handler.collect::<Vec<EventRawRow>>().await);
    }
    assert_eq!(
        rows.len(),
        6,
        "all six events exported across three batches"
    );
    rows.sort_by_key(|row| row.sequence_num);
    let turn_numbers: Vec<i64> = rows.iter().map(|row| row.turn_number).collect();
    assert_eq!(
        turn_numbers,
        vec![1, 1, 1, 2, 2, 2],
        "later-batch events keep the correct turn from the full-prefix count"
    );

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_dim_sessions_supersedes_on_update_db() -> TestResult<()> {
    // Pins: re-exporting a mutated session emits a row with a strictly higher
    // export_version (its new updated_at), so ReplacingMergeTree keeps the latest.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;

    let mock = Mock::new();
    let exporter = exporter(pool.clone(), &mock);

    let first_handler = mock.add(handlers::record::<DimSessionRow>());
    exporter.export_dim_sessions().await?;
    let first: Vec<DimSessionRow> = first_handler.collect().await;
    assert_eq!(first.len(), 1, "the seeded session is exported once");
    let first_version = first[0].export_version;

    // Mutate the session the way the store does (explicit updated_at bump).
    sqlx::query("UPDATE sessions SET status = 'completed', updated_at = NOW() WHERE id = $1")
        .bind(session)
        .execute(&pool)
        .await?;

    let second_handler = mock.add(handlers::record::<DimSessionRow>());
    exporter.export_dim_sessions().await?;
    let second: Vec<DimSessionRow> = second_handler.collect().await;
    assert_eq!(
        second.len(),
        1,
        "the overlap window re-reads the mutated row"
    );
    assert_eq!(second[0].session_id, session);
    assert_eq!(second[0].status, "completed");
    assert!(
        second[0].export_version > first_version,
        "the re-export must carry a higher export_version to supersede the prior copy"
    );

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn analytics_export_events_cursor_resumes_after_restart_db() -> TestResult<()> {
    // Pins: a second export pass resumes from the persisted cursor rather than
    // replaying history — only rows past the overlap-rewound cursor are re-read.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_session(&pool, tenant, session).await?;

    let base = Utc::now() - Duration::days(1);
    let empty = json!({"data": {}});
    insert_event(
        &pool,
        tenant,
        session,
        1,
        "BrainResponse",
        empty.clone(),
        base,
    )
    .await?;
    insert_event(
        &pool,
        tenant,
        session,
        2,
        "BrainResponse",
        empty.clone(),
        base + Duration::seconds(10),
    )
    .await?;
    insert_event(
        &pool,
        tenant,
        session,
        3,
        "BrainResponse",
        empty.clone(),
        base + Duration::seconds(20),
    )
    .await?;

    let mock = Mock::new();
    let exporter = exporter(pool.clone(), &mock);

    let first_handler = mock.add(handlers::record::<EventRawRow>());
    exporter.export_events().await?;
    let first: Vec<EventRawRow> = first_handler.collect().await;
    assert_eq!(first.len(), 3, "the first pass backfills all three events");

    // A new event well past the two-second overlap window.
    insert_event(
        &pool,
        tenant,
        session,
        4,
        "BrainResponse",
        empty,
        base + Duration::seconds(120),
    )
    .await?;

    let second_handler = mock.add(handlers::record::<EventRawRow>());
    exporter.export_events().await?;
    let second: Vec<EventRawRow> = second_handler.collect().await;

    let second_seqs: Vec<i64> = second.iter().map(|row| row.sequence_num).collect();
    assert!(
        second_seqs.contains(&4),
        "the new event must be exported on resume"
    );
    assert!(
        !second_seqs.contains(&1) && !second_seqs.contains(&2),
        "events older than the overlap window must not be re-read: {second_seqs:?}"
    );

    pool.close().await;
    Ok(())
}
