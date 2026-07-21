//! Live end-to-end coverage: Postgres → analytics exporter → real ClickHouse →
//! analytics query backend.
//!
//! Pins the pieces neither the mock-ClickHouse db lane nor the offline SQL
//! snapshots can: the exporter DDL is accepted by a real server, RowBinary
//! inserts round-trip, the ClickHouse-dialect dataset SQL (FINAL, uniqExactIf,
//! LIMIT 1 BY, quantile/aggregations, toUnixTimestamp64Micro projections)
//! executes and decodes, and the tenant purge empties every table.
//!
//! Run with the compose services:
//! `docker compose --profile clickhouse up -d clickhouse` (Postgres up too),
//! then `MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 MOA_DATABASE_URL=... cargo nextest \
//! run -p moa-orchestrator --run-ignored all -E 'test(analytics_clickhouse_roundtrip_e2e)'`.

use chrono::{DateTime, Duration, Utc};
use moa_analytics::{AnalyticsClickHouseClient, AnalyticsService};
use moa_core::config::ClickHouseConfig;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCell, AnalyticsDimension, AnalyticsFilter,
    AnalyticsFilterOperator, AnalyticsMeasure, AnalyticsQueryRequest,
};
use moa_core::{types::identifiers::TenantId, wire::analytics::AnalyticsQueryResponse};
use moa_orchestrator::analytics_export::AnalyticsExporter;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

type TestResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
#[ignore = "requires local ClickHouse (docker compose --profile clickhouse) and MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1"]
async fn analytics_clickhouse_roundtrip_e2e() -> TestResult<()> {
    if std::env::var("MOA_RUN_CLICKHOUSE_DOCKER_TESTS").as_deref() != Ok("1") {
        return Err("MOA_RUN_CLICKHOUSE_DOCKER_TESTS=1 is required for this test".into());
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant = Uuid::now_v7();
    let session = Uuid::now_v7();
    seed_two_turn_session(&pool, tenant, session).await?;

    // Isolated ClickHouse database per run so concurrent runs cannot collide.
    let config = ClickHouseConfig {
        url: std::env::var("MOA_CLICKHOUSE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10061".to_string()),
        database: format!("moa_analytics_e2e_{}", Uuid::now_v7().simple()),
        user: Some(std::env::var("MOA_CLICKHOUSE_USER").unwrap_or_else(|_| "moa".to_string())),
        password: Some(
            std::env::var("MOA_CLICKHOUSE_PASSWORD").unwrap_or_else(|_| "dev".to_string()),
        ),
        ..ClickHouseConfig::default()
    };

    // Production path: bootstrap the schema and run one full export pass
    // (dims, events with turn stamping, windowed facts).
    let exporter = AnalyticsExporter::from_config(pool.clone(), &config);
    exporter.ensure_clickhouse_schema().await?;
    exporter.run_one_pass().await?;

    let client = AnalyticsClickHouseClient::connect(&config);
    let service = AnalyticsService::clickhouse();
    let tenant_id = TenantId::from(tenant);

    // turns dataset: per-model cost rollup from the exporter-computed
    // turn_fact (FINAL path). Seeded turns cost 5 + 8 cents on model "claude".
    let turns = service
        .query_clickhouse(
            &client,
            AnalyticsQueryRequest {
                dataset: "turns".to_string(),
                tenant_id: Some(tenant_id),
                dimensions: vec![AnalyticsDimension {
                    field: "model".to_string(),
                    alias: None,
                }],
                measures: vec![AnalyticsMeasure {
                    field: Some("cost_cents".to_string()),
                    aggregation: AnalyticsAggregation::Sum,
                    alias: Some("total_cost".to_string()),
                }],
                filters: vec![recent_time_window("finished_at")],
                order_by: Vec::new(),
                limit: Some(10),
            },
        )
        .await?;
    assert_eq!(turns.rows.len(), 1, "one model group expected: {turns:?}");
    assert_eq!(
        turns.rows[0][0],
        AnalyticsCell::String("claude".to_string())
    );
    assert_numeric(&turns, 0, 1, 13.0);

    // sessions dataset: exercises dim_sessions FINAL joined to the
    // duplicate-tolerant events_raw rollup (uniqExactIf tool counts).
    let sessions = service
        .query_clickhouse(
            &client,
            AnalyticsQueryRequest {
                dataset: "sessions".to_string(),
                tenant_id: Some(tenant_id),
                dimensions: Vec::new(),
                measures: vec![
                    AnalyticsMeasure {
                        field: None,
                        aggregation: AnalyticsAggregation::Count,
                        alias: Some("session_count".to_string()),
                    },
                    AnalyticsMeasure {
                        field: Some("tool_call_count".to_string()),
                        aggregation: AnalyticsAggregation::Sum,
                        alias: Some("tool_calls".to_string()),
                    },
                ],
                filters: vec![recent_time_window("created_at")],
                order_by: Vec::new(),
                limit: Some(10),
            },
        )
        .await?;
    assert_eq!(sessions.rows.len(), 1, "one rollup row: {sessions:?}");
    assert_numeric(&sessions, 0, 0, 1.0);
    assert_numeric(&sessions, 0, 1, 2.0);

    // Offboarding purge empties every analytics table for the tenant.
    client.purge_tenant(tenant).await?;
    let after_purge = service
        .query_clickhouse(
            &client,
            AnalyticsQueryRequest {
                dataset: "turns".to_string(),
                tenant_id: Some(tenant_id),
                dimensions: Vec::new(),
                measures: vec![AnalyticsMeasure {
                    field: None,
                    aggregation: AnalyticsAggregation::Count,
                    alias: Some("turn_count".to_string()),
                }],
                filters: vec![recent_time_window("finished_at")],
                order_by: Vec::new(),
                limit: Some(10),
            },
        )
        .await?;
    let purged_empty = after_purge.rows.is_empty()
        || matches!(numeric_cell(&after_purge, 0, 0), Some(count) if count == 0.0);
    assert!(
        purged_empty,
        "purge must remove tenant rows: {after_purge:?}"
    );

    Ok(())
}

/// Asserts a numeric response cell regardless of integer/float JSON decoding.
fn assert_numeric(response: &AnalyticsQueryResponse, row: usize, col: usize, expected: f64) {
    let value = numeric_cell(response, row, col)
        .unwrap_or_else(|| panic!("cell [{row}][{col}] should be numeric: {response:?}"));
    assert!(
        (value - expected).abs() < f64::EPSILON,
        "cell [{row}][{col}] = {value}, expected {expected}: {response:?}"
    );
}

fn numeric_cell(response: &AnalyticsQueryResponse, row: usize, col: usize) -> Option<f64> {
    match response.rows.get(row)?.get(col)? {
        AnalyticsCell::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn recent_time_window(field: &str) -> AnalyticsFilter {
    AnalyticsFilter {
        field: field.to_string(),
        operator: AnalyticsFilterOperator::Between,
        value: Some(AnalyticsCell::Json(json!([
            (Utc::now() - Duration::days(2)).to_rfc3339(),
            (Utc::now() + Duration::days(1)).to_rfc3339(),
        ]))),
    }
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

#[allow(clippy::too_many_arguments)]
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

/// Two turns: each a ToolCall + ToolResult + BrainResponse; costs 5 and 8.
async fn seed_two_turn_session(pool: &PgPool, tenant: Uuid, session: Uuid) -> TestResult<()> {
    seed_session(pool, tenant, session).await?;
    let base = Utc::now() - Duration::days(1);
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
            json!({"data": {"model": "claude", "duration_ms": 100.0,
                "input_tokens_uncached": 10, "input_tokens_cache_write": 2,
                "input_tokens_cache_read": 3, "output_tokens": 7, "cost_cents": 5}}),
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
            json!({"data": {"model": "claude", "duration_ms": 200.0,
                "input_tokens_uncached": 20, "input_tokens_cache_write": 0,
                "input_tokens_cache_read": 1, "output_tokens": 9, "cost_cents": 8}}),
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
            base + Duration::milliseconds(offset_ms),
        )
        .await?;
    }
    Ok(())
}
