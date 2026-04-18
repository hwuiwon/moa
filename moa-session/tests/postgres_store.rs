mod shared;

use std::future::Future;
use std::time::Duration;

use chrono::Utc;
use moa_core::{
    Event, ModelId, SessionMeta, SessionStore, ToolCallId, ToolOutput, UserId, WorkspaceId,
};
use moa_session::{PostgresSessionStore, testing};
use sqlx::PgPool;
use sqlx::types::Json;
use uuid::Uuid;

async fn create_test_store() -> (PostgresSessionStore, String, String) {
    testing::create_isolated_test_store()
        .await
        .expect("postgres store")
}

async fn cleanup_schema(database_url: &str, schema_name: &str) {
    testing::cleanup_test_schema(database_url, schema_name)
        .await
        .expect("drop schema");
}

async fn with_test_store<F, Fut>(test: F)
where
    F: FnOnce(PostgresSessionStore) -> Fut,
    Fut: Future<Output = ()>,
{
    let (store, database_url, schema_name) = create_test_store().await;
    test(store.clone()).await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!("\"{}\".\"{}\"", schema_name, table_name)
}

#[tokio::test]
#[ignore]
async fn postgres_shared_session_store_contract() {
    with_test_store(|store| async move {
        shared::test_create_and_get_session(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_emit_and_get_events(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_pending_signals(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_event_search(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_list_sessions_with_filter(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_workspace_cost_since(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_session_status_update(&store).await;
    })
    .await;
    with_test_store(|store| async move {
        shared::test_approval_rules(&store).await;
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn postgres_event_payloads_round_trip_as_jsonb() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("pg-jsonb"),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    let tool_uuid = Uuid::now_v7();
    let tool_id = moa_core::ToolCallId(tool_uuid);
    let output = ToolOutput::json(
        "structured",
        serde_json::json!({
            "nested": { "value": 42, "ok": true },
            "items": ["a", "b", "c"]
        }),
        Duration::from_millis(25),
    );
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: Some("toolu_jsonb".to_string()),
                output: output.clone(),
                original_output_tokens: None,
                success: true,
                duration_ms: 25,
            },
        )
        .await
        .expect("emit tool result");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let payload: serde_json::Value = sqlx::query_scalar(&format!(
        "SELECT payload FROM {} LIMIT 1",
        qualified(&schema_name, "events")
    ))
    .fetch_one(&pool)
    .await
    .expect("fetch payload");
    let jsonb_type: String = sqlx::query_scalar(&format!(
        "SELECT pg_typeof(payload)::text FROM {} LIMIT 1",
        qualified(&schema_name, "events")
    ))
    .fetch_one(&pool)
    .await
    .expect("fetch payload type");

    assert_eq!(jsonb_type, "jsonb");
    assert_eq!(payload["type"], "ToolResult");
    assert_eq!(payload["data"]["tool_id"], tool_id.to_string());
    assert_eq!(
        payload["data"]["output"]["structured"]["nested"]["value"],
        serde_json::json!(42)
    );

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_session_ids_are_native_uuid_and_concurrent_emits_are_serialized() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("pg-concurrency"),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let id_type: String = sqlx::query_scalar(&format!(
        "SELECT pg_typeof(id)::text FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch id type");
    assert_eq!(id_type, "uuid");

    let mut tasks = Vec::new();
    for index in 0..10 {
        let store = store.clone();

        tasks.push(tokio::spawn(async move {
            store
                .emit_event(
                    session_id,
                    Event::UserMessage {
                        text: format!("parallel {index}"),
                        attachments: vec![],
                    },
                )
                .await
        }));
    }

    let mut sequences = Vec::new();
    for task in tasks {
        sequences.push(task.await.expect("join task").expect("emit event"));
    }
    sequences.sort_unstable();
    assert_eq!(sequences, (0..10).collect::<Vec<_>>());

    let event_count: i64 = sqlx::query_scalar(&format!(
        "SELECT event_count FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch event_count");
    assert_eq!(event_count, 10);

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_connection_retry_surfaces_final_failure() {
    let error = match PostgresSessionStore::new("postgres://127.0.0.1:1/moa_test").await {
        Ok(_) => panic!("invalid endpoint should fail"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("after 3 attempts"));
}

#[tokio::test]
#[ignore]
async fn postgres_trigger_populates_generated_session_rollups() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("analytics-ws"),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    for (uncached, cache_write, cache_read, output, cost) in [
        (10usize, 5usize, 15usize, 4usize, 20u32),
        (20usize, 0usize, 10usize, 6usize, 40u32),
        (0usize, 5usize, 5usize, 3usize, 10u32),
    ] {
        store
            .emit_event(
                session_id,
                Event::BrainResponse {
                    text: "turn".to_string(),
                    thought_signature: None,
                    model: "test-model".into(),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: uncached,
                    input_tokens_cache_write: cache_write,
                    input_tokens_cache_read: cache_read,
                    output_tokens: output,
                    cost_cents: cost,
                    duration_ms: 100,
                },
            )
            .await
            .expect("emit brain response");
    }

    let summary = store
        .get_session_summary(session_id)
        .await
        .expect("load session summary");
    assert_eq!(summary.turn_count, 3);
    assert_eq!(summary.total_input_tokens, 70);
    assert_eq!(summary.total_output_tokens, 13);
    assert_eq!(summary.total_cost_cents, 70);
    assert!(approx_eq(summary.cache_hit_rate, 30.0 / 70.0, 1e-9));

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let (turn_count, cache_hit_rate): (i64, f64) = sqlx::query_as(&format!(
        "SELECT turn_count, cache_hit_rate FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch generated session columns");
    assert_eq!(turn_count, 3);
    assert!(approx_eq(cache_hit_rate, 30.0 / 70.0, 1e-9));

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_session_summary_tracks_model_tier_costs() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("tiered-costs-ws"),
            user_id: UserId::new("user"),
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    let tool_id = ToolCallId::new();
    store
        .emit_event(
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "echo hi" }),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("hi", Duration::from_millis(10)),
                original_output_tokens: None,
                success: true,
                duration_ms: 10,
            },
        )
        .await
        .expect("emit tool result");
    store
        .emit_event(
            session_id,
            Event::BrainResponse {
                text: "main turn".to_string(),
                thought_signature: None,
                model: "claude-sonnet-4-6".into(),
                model_tier: moa_core::ModelTier::Main,
                input_tokens_uncached: 12,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 6,
                cost_cents: 20,
                duration_ms: 30,
            },
        )
        .await
        .expect("emit brain response");
    store
        .emit_event(
            session_id,
            Event::Checkpoint {
                summary: "summarized prior turns".to_string(),
                events_summarized: 2,
                token_count: 8,
                model: "claude-haiku-4-5".into(),
                model_tier: moa_core::ModelTier::Auxiliary,
                input_tokens: 9,
                output_tokens: 4,
                cost_cents: 6,
            },
        )
        .await
        .expect("emit checkpoint");

    let summary = store
        .get_session_summary(session_id)
        .await
        .expect("load session summary");
    assert_eq!(summary.total_cost_cents, 26);
    assert_eq!(summary.main_cost_cents, 20);
    assert_eq!(summary.auxiliary_cost_cents, 6);

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let (main_cost_cents, auxiliary_cost_cents): (i64, i64) = sqlx::query_as(&format!(
        "SELECT main_cost_cents, auxiliary_cost_cents FROM {} WHERE id = $1",
        qualified(&schema_name, "session_summary")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query session_summary view");
    assert_eq!(main_cost_cents, 20);
    assert_eq!(auxiliary_cost_cents, 6);

    let tool_model_tier: String = sqlx::query_scalar(&format!(
        "SELECT model_tier FROM {} WHERE session_id = $1 LIMIT 1",
        qualified(&schema_name, "tool_call_analytics")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("query tool_call_analytics view");
    assert_eq!(tool_model_tier, "main");

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_trigger_failure_rolls_back_insert() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("rollback-ws"),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let error = sqlx::query(&format!(
        "INSERT INTO {} \
         (id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count) \
         VALUES ($1, $2, $3, $4, $5, $6, NULL, NULL, NULL)",
        qualified(&schema_name, "events")
    ))
    .bind(Uuid::now_v7())
    .bind(session_id.0)
    .bind(0_i64)
    .bind("BrainResponse")
    .bind(Json(serde_json::json!({
        "type": "BrainResponse",
        "data": {
            "text": "bad",
            "model": "test-model",
            "input_tokens_uncached": "not-a-number",
            "input_tokens_cache_write": 0,
            "input_tokens_cache_read": 0,
            "output_tokens": 1,
            "cost_cents": 1,
            "duration_ms": 1
        }
    })))
    .bind(Utc::now())
    .execute(&pool)
    .await
    .expect_err("malformed payload should fail inside trigger");
    assert!(
        error.to_string().contains("invalid input syntax"),
        "unexpected trigger error: {error}"
    );

    let event_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE session_id = $1",
        qualified(&schema_name, "events")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("count events");
    let session_event_count: i64 = sqlx::query_scalar(&format!(
        "SELECT event_count FROM {} WHERE id = $1",
        qualified(&schema_name, "sessions")
    ))
    .bind(session_id.0)
    .fetch_one(&pool)
    .await
    .expect("fetch session event_count");
    assert_eq!(event_count, 0);
    assert_eq!(session_event_count, 0);

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_tool_call_summary_view_reports_percentiles() {
    let (store, database_url, schema_name) = create_test_store().await;
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("tool-stats-ws"),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    for (duration_ms, success) in [
        (100_u64, true),
        (200_u64, true),
        (300_u64, true),
        (400_u64, true),
        (500_u64, false),
    ] {
        let tool_id = ToolCallId::new();
        store
            .emit_event(
                session_id,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "bash".to_string(),
                    input: serde_json::json!({ "cmd": "true" }),
                    hand_id: None,
                },
            )
            .await
            .expect("emit tool call");
        store
            .emit_event(
                session_id,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text("ok", Duration::from_millis(duration_ms)),
                    original_output_tokens: None,
                    success,
                    duration_ms,
                },
            )
            .await
            .expect("emit tool result");
    }

    let workspace_rows = store
        .list_tool_call_summaries(Some(&WorkspaceId::new("tool-stats-ws")))
        .await
        .expect("load workspace tool summary");
    let summary = workspace_rows
        .iter()
        .find(|row| row.tool_name == "bash")
        .expect("bash summary");
    assert_eq!(summary.call_count, 5);
    assert!(approx_eq(summary.avg_duration_ms, 300.0, 1e-9));
    assert!(approx_eq(summary.p50_ms, 300.0, 1e-9));
    assert!(approx_eq(summary.p95_ms, 480.0, 1e-9));
    assert!(approx_eq(summary.success_rate, 0.8, 1e-9));

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let row: (i64, f64, f64) = sqlx::query_as(&format!(
        "SELECT call_count, p50_ms, p95_ms FROM {} WHERE tool_name = $1",
        qualified(&schema_name, "tool_call_summary")
    ))
    .bind("bash")
    .fetch_one(&pool)
    .await
    .expect("query tool_call_summary view");
    assert_eq!(row.0, 5);
    assert!(approx_eq(row.1, 300.0, 1e-9));
    assert!(approx_eq(row.2, 480.0, 1e-9));

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

#[tokio::test]
#[ignore]
async fn postgres_materialized_analytics_views_refresh() {
    let (store, database_url, schema_name) = create_test_store().await;
    let workspace_id = WorkspaceId::new("mv-ws");
    let first_session_id = store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create first session");
    let second_session_id = store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            model: ModelId::new("test-model"),
            ..SessionMeta::default()
        })
        .await
        .expect("create second session");

    let tool_id = ToolCallId::new();
    store
        .emit_event(
            first_session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "file_read".to_string(),
                input: serde_json::json!({ "path": "README.md" }),
                hand_id: None,
            },
        )
        .await
        .expect("emit tool call");
    store
        .emit_event(
            first_session_id,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("ok", Duration::from_millis(120)),
                original_output_tokens: None,
                success: true,
                duration_ms: 120,
            },
        )
        .await
        .expect("emit tool result");
    for (session_id, llm_ms, uncached, cache_read, output, cost) in [
        (
            first_session_id,
            250_u64,
            15_usize,
            5_usize,
            4_usize,
            12_u32,
        ),
        (first_session_id, 175_u64, 8_usize, 2_usize, 3_usize, 6_u32),
        (
            second_session_id,
            300_u64,
            20_usize,
            10_usize,
            6_usize,
            18_u32,
        ),
    ] {
        store
            .emit_event(
                session_id,
                Event::BrainResponse {
                    text: "turn".to_string(),
                    thought_signature: None,
                    model: "test-model".into(),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: uncached,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: cache_read,
                    output_tokens: output,
                    cost_cents: cost,
                    duration_ms: llm_ms,
                },
            )
            .await
            .expect("emit brain response");
    }

    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh materialized analytics views");

    let turn_metrics = store
        .list_session_turn_metrics(first_session_id)
        .await
        .expect("load session turn metrics");
    assert_eq!(turn_metrics.len(), 2);
    assert_eq!(turn_metrics[0].turn_number, 1);
    assert!(approx_eq(turn_metrics[0].llm_ms, 250.0, 1e-9));
    assert!(approx_eq(turn_metrics[0].tool_ms, 120.0, 1e-9));
    assert_eq!(turn_metrics[0].tool_call_count, 1);
    assert_eq!(turn_metrics[0].total_input_tokens, 20);

    let workspace_summary = store
        .get_workspace_stats(&workspace_id, 30)
        .await
        .expect("load workspace stats");
    assert_eq!(workspace_summary.session_count, 2);
    assert_eq!(workspace_summary.turn_count, 3);
    assert_eq!(workspace_summary.total_input_tokens, 60);
    assert_eq!(workspace_summary.total_cache_read_tokens, 17);
    assert_eq!(workspace_summary.total_output_tokens, 13);
    assert_eq!(workspace_summary.total_cost_cents, 36);

    let daily_metrics = store
        .list_cache_daily_metrics(&workspace_id, 30)
        .await
        .expect("load cache daily metrics");
    assert_eq!(daily_metrics.len(), 1);
    assert_eq!(daily_metrics[0].session_count, 2);
    assert_eq!(daily_metrics[0].turn_count, 3);

    let pool = PgPool::connect(&database_url)
        .await
        .expect("postgres inspection pool");
    let session_count: i64 = sqlx::query_scalar(&format!(
        "SELECT session_count FROM {} WHERE workspace_id = $1",
        qualified(&schema_name, "daily_workspace_metrics")
    ))
    .bind(workspace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("query daily workspace metrics");
    assert_eq!(session_count, 2);

    pool.close().await;
    drop(store);
    cleanup_schema(&database_url, &schema_name).await;
}

fn approx_eq(left: f64, right: f64, epsilon: f64) -> bool {
    (left - right).abs() <= epsilon
}
