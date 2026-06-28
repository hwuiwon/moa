//! DB-backed tests for direct edge read routes.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use chrono::{Duration, Utc};
use moa_authz::{FgaClient, FgaConfig};
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use moa_core::wire::analytics::{SessionSearchResponse, ToolStatsResponse};
use moa_core::wire::lineage::LineageQueryResponse;
use moa_core::{
    Event, MoaConfig, ModelId, SessionActorRef, SessionMeta, SessionStore, TenantId, ToolCallId,
    ToolOutput,
};
use moa_edge::proxy::OrchestratorProxy;
use moa_edge::routes::{self, AppState, KnowledgeWebhookEdgeConfig};
use moa_session::{PostgresSessionStore, testing};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[derive(Clone)]
struct FixedAuth {
    identity: Identity,
}

#[async_trait]
impl AuthProvider for FixedAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Ok(self.identity.clone())
    }

    fn name(&self) -> &'static str {
        "fixed-test"
    }

    fn requires_credentials(&self) -> bool {
        false
    }
}

#[derive(Clone)]
struct FgaMockState {
    allowed: bool,
    checks: Arc<Mutex<Vec<Value>>>,
}

struct FgaMock {
    client: FgaClient,
    checks: Arc<Mutex<Vec<Value>>>,
    server: JoinHandle<()>,
}

struct EdgeServer {
    base_url: String,
    server: JoinHandle<()>,
}

async fn fga_check(
    State(state): State<FgaMockState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    state.checks.lock().await.push(body);
    axum::Json(json!({ "allowed": state.allowed }))
}

async fn start_fga_mock(allowed: bool) -> FgaMock {
    let checks = Arc::new(Mutex::new(Vec::new()));
    let state = FgaMockState {
        allowed,
        checks: checks.clone(),
    };
    let app = Router::new()
        .route("/stores/{store_id}/check", post(fga_check))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FGA mock");
    let addr = listener.local_addr().expect("read FGA mock addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve FGA mock");
    });
    let client = FgaClient::new(FgaConfig {
        url: format!("http://{addr}"),
        preshared_key: "test-key".to_string(),
        store_id: "store-1".to_string(),
        model_id: "model-1".to_string(),
        timeout_ms: 5_000,
    })
    .expect("test FGA config should be valid");
    FgaMock {
        client,
        checks,
        server,
    }
}

async fn start_edge(
    store: &PostgresSessionStore,
    database_url: &str,
    schema_name: &str,
    identity: Identity,
    fga: Option<FgaClient>,
) -> EdgeServer {
    let mut config = MoaConfig::default();
    config.database.url = database_url.to_string();
    config.database.schema = Some(schema_name.to_string());
    let state = AppState {
        config: Arc::new(config),
        auth: Arc::new(FixedAuth { identity }),
        fga: fga.map(Arc::new),
        auth0_webhook_secret: None,
        knowledge_webhooks: KnowledgeWebhookEdgeConfig::default(),
        pool: Arc::new(store.pool().clone()),
        session_store: Arc::new(store.clone()),
        proxy: Arc::new(
            OrchestratorProxy::new("http://127.0.0.1:1")
                .expect("proxy URL should be syntactically valid"),
        ),
    };
    let app = routes::router(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind edge test server");
    let addr = listener.local_addr().expect("read edge test addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve edge test router");
    });
    EdgeServer {
        base_url: format!("http://{addr}"),
        server,
    }
}

fn identity(identity_type: IdentityType, tenant_id: TenantId) -> Identity {
    Identity {
        identity_type,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn session_meta(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

async fn create_test_store() -> (PostgresSessionStore, String, String) {
    testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store")
}

async fn cleanup_test_store(
    store: PostgresSessionStore,
    database_url: String,
    schema_name: String,
) {
    store.pool().close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated Postgres schema");
}

async fn stop_server(server: JoinHandle<()>) {
    server.abort();
    let _ = server.await;
}

async fn stop_fga_mock(mock_server: JoinHandle<()>) {
    mock_server.abort();
    let _ = mock_server.await;
}

async fn seed_tool_call(store: &PostgresSessionStore, tenant_id: TenantId, tool_name: &str) {
    let session_id = store
        .create_session(session_meta(tenant_id))
        .await
        .expect("create tool stats session");
    let tool_id = ToolCallId(Uuid::now_v7());
    store
        .emit_event(
            session_id,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: tool_name.to_string(),
                input: json!({ "arg": tool_name }),
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
                output: ToolOutput::text("ok", std::time::Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
            },
        )
        .await
        .expect("emit tool result");
}

async fn insert_lineage_row(
    pool: &PgPool,
    tenant_id: TenantId,
    session_id: Uuid,
    user_id: &str,
    record_kind: i16,
    answer_text: &str,
) -> Uuid {
    let turn_id = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage
            (turn_id, session_id, user_id, storage_partition_id, ts, tier, record_kind, payload, answer_text, integrity_hash)
        VALUES
            ($1, $2, $3, $4, $5, 1, $6, $7, $8, $9)
        "#,
    )
    .bind(turn_id)
    .bind(session_id)
    .bind(user_id)
    .bind(tenant_id.to_string())
    .bind(Utc::now())
    .bind(record_kind)
    .bind(json!({ "answer": answer_text }))
    .bind(answer_text)
    .bind(vec![record_kind as u8; 32])
    .execute(pool)
    .await
    .expect("insert lineage row");
    turn_id
}

async fn delete_lineage_rows(pool: &PgPool, tenants: &[TenantId]) {
    for tenant in tenants {
        sqlx::query("DELETE FROM analytics.turn_lineage WHERE storage_partition_id = $1")
            .bind(tenant.to_string())
            .execute(pool)
            .await
            .expect("delete test lineage rows");
    }
}

#[tokio::test]
async fn authz_gate_runs_before_session_stats_read_db() {
    // Pins: protected direct reads fail closed at authz before reading session data.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::User, tenant_id),
        None,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/analytics/session-stats", edge.base_url))
        .json(&json!({ "session_id": Uuid::now_v7() }))
        .send()
        .await
        .expect("send session stats request");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    stop_server(edge.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn tool_stats_injects_user_tenant_and_allows_service_wide_reads_db() {
    // Pins: user tool stats are forced to the authenticated tenant; service requests may omit tenant_id.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    seed_tool_call(&store, tenant_a, "tenant_a_tool").await;
    seed_tool_call(&store, tenant_b, "tenant_b_tool").await;

    let fga = start_fga_mock(true).await;
    let user_edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::User, tenant_a),
        Some(fga.client.clone()),
    )
    .await;
    let user_response: ToolStatsResponse = reqwest::Client::new()
        .post(format!("{}/v1/analytics/tool-stats", user_edge.base_url))
        .json(&json!({ "tenant_id": tenant_b }))
        .send()
        .await
        .expect("send user tool stats request")
        .error_for_status()
        .expect("user tool stats should succeed")
        .json()
        .await
        .expect("decode user tool stats response");

    assert_eq!(user_response.tenant_id, Some(tenant_a));
    assert_eq!(user_response.rows.len(), 1);
    assert_eq!(user_response.rows[0].tool_name, "tenant_a_tool");

    let service_edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Service, tenant_a),
        Some(fga.client.clone()),
    )
    .await;
    let service_response: ToolStatsResponse = reqwest::Client::new()
        .post(format!("{}/v1/analytics/tool-stats", service_edge.base_url))
        .json(&json!({}))
        .send()
        .await
        .expect("send service tool stats request")
        .error_for_status()
        .expect("service deployment-wide tool stats should succeed")
        .json()
        .await
        .expect("decode service tool stats response");

    let service_tools = service_response
        .rows
        .iter()
        .map(|row| row.tool_name.as_str())
        .collect::<HashSet<_>>();
    assert_eq!(service_response.tenant_id, None);
    assert_eq!(service_tools.len(), 2);
    assert!(service_tools.contains("tenant_a_tool"));
    assert!(service_tools.contains("tenant_b_tool"));

    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 2);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{tenant_a}"))
    );
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("operator"));
    assert_eq!(checks[1]["tuple_key"]["relation"], json!("admin"));

    stop_server(user_edge.server).await;
    stop_server(service_edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn session_search_uses_configured_schema_db() {
    // Pins: direct session search builds its store with the configured schema, not the default schema.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let session_id = store
        .create_session(session_meta(tenant_id))
        .await
        .expect("create search session");
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: "schema only needle".to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit searchable event");

    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::User, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let response: SessionSearchResponse = reqwest::Client::new()
        .post(format!("{}/v1/analytics/session-search", edge.base_url))
        .json(&json!({
            "query": "needle",
            "limit": 5
        }))
        .send()
        .await
        .expect("send session search request")
        .error_for_status()
        .expect("session search should succeed")
        .json()
        .await
        .expect("decode session search response");

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].session_id, session_id);
    assert_eq!(response.results[0].snippet, "UserMessage");

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn lineage_query_uses_typed_filters_and_rejects_legacy_sql_db() {
    // Pins: direct lineage query reads only typed `analytics.turn_lineage` filters scoped to one tenant.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let session_a = Uuid::now_v7();
    let session_b = Uuid::now_v7();
    let turn_a = insert_lineage_row(
        store.pool(),
        tenant_a,
        session_a,
        "user-a",
        7,
        "tenant-a-lineage",
    )
    .await;
    insert_lineage_row(
        store.pool(),
        tenant_b,
        session_b,
        "user-b",
        7,
        "tenant-b-lineage",
    )
    .await;

    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::User, tenant_a),
        Some(fga.client.clone()),
    )
    .await;

    let response: LineageQueryResponse = reqwest::Client::new()
        .post(format!("{}/v1/lineage/query", edge.base_url))
        .json(&json!({
            "filters": {
                "record_kind": 7,
                "from_time": (Utc::now() - Duration::minutes(5)).to_rfc3339()
            },
            "order": "timestamp_asc",
            "limit": 10
        }))
        .send()
        .await
        .expect("send typed lineage query")
        .error_for_status()
        .expect("typed lineage query should succeed")
        .json()
        .await
        .expect("decode typed lineage response");

    assert_eq!(response.rows.len(), 1);
    assert_eq!(response.rows[0].turn_id, turn_a);
    assert_eq!(response.rows[0].session_id.map(|id| id.0), Some(session_a));
    assert_eq!(response.rows[0].tenant_id, Some(tenant_a));
    assert_eq!(
        response.rows[0].summary.as_deref(),
        Some("tenant-a-lineage")
    );

    let legacy_response = reqwest::Client::new()
        .post(format!("{}/v1/lineage/query", edge.base_url))
        .json(&json!({
            "sql": "SELECT * FROM pg_catalog.pg_tables",
            "since": "1 hour"
        }))
        .send()
        .await
        .expect("send legacy lineage SQL query");
    assert_eq!(legacy_response.status(), StatusCode::BAD_REQUEST);

    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{tenant_a}"))
    );
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("operator"));

    delete_lineage_rows(store.pool(), &[tenant_a, tenant_b]).await;
    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}
