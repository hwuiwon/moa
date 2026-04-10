#![cfg(feature = "postgres")]

mod shared;

use std::future::Future;
use std::time::Duration;

use moa_core::{Event, SessionMeta, SessionStore, ToolOutput, UserId, WorkspaceId};
use moa_session::PostgresSessionStore;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

async fn create_test_store() -> (PostgresSessionStore, String, String) {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for ignored tests");
    let schema_name = format!("moa_test_{}", Uuid::new_v4().simple());
    let store = PostgresSessionStore::new_in_schema(&database_url, &schema_name)
        .await
        .expect("postgres store");
    (store, database_url, schema_name)
}

async fn cleanup_schema(database_url: &str, schema_name: &str) {
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("admin postgres pool");
    let query = format!(
        "DROP SCHEMA IF EXISTS {} CASCADE",
        quote_identifier(schema_name)
    );
    sqlx::query(&query)
        .execute(&pool)
        .await
        .expect("drop schema");
    pool.close().await;
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

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(table_name)
    )
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
            model: "test-model".to_string(),
            ..SessionMeta::default()
        })
        .await
        .expect("create session");

    let tool_id = Uuid::new_v4();
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
                output: output.clone(),
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
            model: "test-model".to_string(),
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
        let session_id = session_id.clone();
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
