//! DB-backed tests for direct edge read routes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use chrono::{Duration, Utc};
use moa_authz::{FgaClient, FgaConfig};
use moa_config::MoaConfig;
use moa_config::{OAuthClientConfig, OAuthClientType, OAuthServerConfig};
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use moa_core::{
    events::Event, traits::SessionStore, types::agent::AgentContext,
    types::contact::SessionActorRef, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::session::SessionMeta,
    types::tools::ToolOutput,
};
use moa_edge::proxy::OrchestratorProxy;
use moa_edge::routes::{self, AppState, KnowledgeWebhookEdgeConfig};
use moa_session::{PostgresSessionStore, testing};
use moa_wire::analytics::{AnalyticsCatalogResponse, AnalyticsCell, AnalyticsQueryResponse};
use moa_wire::lineage::LineageQueryResponse;
use moa_wire::tenants::{TenantPurgeStatus, TenantPurgeStatusResponse};
use reqwest::{Method as RequestMethod, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

#[path = "direct_read_routes_db/graceful_shutdown_db.rs"]
mod graceful_shutdown_db;
#[path = "direct_read_routes_db/mcp_db.rs"]
mod mcp_db;
#[path = "direct_read_routes_db/session_messages_db.rs"]
mod session_messages_db;

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
struct CountingAuth {
    identity: Identity,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl AuthProvider for CountingAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.identity.clone())
    }

    fn name(&self) -> &'static str {
        "counting-test"
    }

    fn requires_credentials(&self) -> bool {
        false
    }
}

#[derive(Clone)]
struct FgaMockState {
    allowed: bool,
    allowed_objects: Arc<Vec<String>>,
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

struct PurgeUpstream {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    server: JoinHandle<()>,
}

// The edge router configures moa-authz's process-global security audit sink.
// Dashboard denial tests hold this narrow lock so one isolated pool is not
// closed while another test's denied authz audit is still using it.
static DASHBOARD_SESSIONS_TEST_LOCK: Mutex<()> = Mutex::const_new(());

const EDGE_OAUTH_CLIENT_ID: &str = "edge-test-client";
const EDGE_OAUTH_RESOURCE: &str = "https://moa.test/mcp";

fn edge_oauth_config() -> OAuthServerConfig {
    OAuthServerConfig {
        issuer: "https://moa.test".to_string(),
        resource: EDGE_OAUTH_RESOURCE.to_string(),
        authorization_request_ttl_seconds: 300,
        authorization_code_ttl_seconds: 60,
        access_token_ttl_seconds: 3600,
        refresh_token_ttl_seconds: 7200,
        clients: vec![OAuthClientConfig {
            client_id: EDGE_OAUTH_CLIENT_ID.to_string(),
            client_type: OAuthClientType::Public,
            redirect_uris: vec!["https://app.example/callback".to_string()],
            scopes: vec!["mcp:read".to_string(), "mcp:write".to_string()],
            client_secret_sha256: None,
        }],
    }
}

async fn fga_check(
    State(state): State<FgaMockState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    let object = body["tuple_key"]["object"].as_str().unwrap_or_default();
    let allowed = state.allowed || state.allowed_objects.iter().any(|value| value == object);
    state.checks.lock().await.push(body);
    axum::Json(json!({ "allowed": allowed }))
}

async fn start_fga_mock(allowed: bool) -> FgaMock {
    start_fga_mock_with_objects(allowed, Vec::new()).await
}

async fn start_fga_mock_with_objects(allowed: bool, allowed_objects: Vec<String>) -> FgaMock {
    let checks = Arc::new(Mutex::new(Vec::new()));
    let state = FgaMockState {
        allowed,
        allowed_objects: Arc::new(allowed_objects),
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
    start_edge_with_upstream(
        store,
        database_url,
        schema_name,
        identity,
        fga,
        "http://127.0.0.1:1",
    )
    .await
}

async fn start_edge_with_upstream(
    store: &PostgresSessionStore,
    database_url: &str,
    schema_name: &str,
    identity: Identity,
    fga: Option<FgaClient>,
    upstream: &str,
) -> EdgeServer {
    start_edge_with_auth_and_upstream(
        store,
        database_url,
        schema_name,
        Arc::new(FixedAuth { identity }),
        fga,
        upstream,
    )
    .await
}

async fn start_edge_with_auth_and_upstream(
    store: &PostgresSessionStore,
    database_url: &str,
    schema_name: &str,
    auth: Arc<dyn AuthProvider>,
    fga: Option<FgaClient>,
    upstream: &str,
) -> EdgeServer {
    start_edge_with_auth_upstream_and_connector_management(
        store,
        database_url,
        schema_name,
        auth,
        fga,
        upstream,
        false,
    )
    .await
}

async fn start_edge_with_auth_upstream_and_connector_management(
    store: &PostgresSessionStore,
    database_url: &str,
    schema_name: &str,
    auth: Arc<dyn AuthProvider>,
    fga: Option<FgaClient>,
    upstream: &str,
    connector_management_enabled: bool,
) -> EdgeServer {
    let mut config = MoaConfig::default();
    config.database.url = database_url.to_string();
    config.database.schema = Some(schema_name.to_string());
    config.auth.oauth = edge_oauth_config();
    let pool = Arc::new(store.pool().clone());
    let oauth_server = Arc::new(
        moa_auth_providers::OAuthServer::from_config(&config.auth.oauth, pool.clone())
            .await
            .expect("bootstrap edge OAuth server"),
    );
    let audit = moa_ocsf::AuditRuntime::start(pool.as_ref().clone())
        .expect("edge test audit runtime should start");
    let state = AppState {
        connector_management_enabled,
        // The audit writer is owned by this test for its lifetime; dropping the
        // runtime aborts it, which is the same ownership the binary has.
        audit: audit.emitter(),
        config: Arc::new(config),
        auth,
        oauth_server,
        oauth_access_tokens: Arc::new(moa_auth_providers::OAuthAccessTokenProvider::new(
            pool.clone(),
        )),
        fga: fga.map(Arc::new),
        knowledge_webhooks: KnowledgeWebhookEdgeConfig::default(),
        pool,
        session_store: Arc::new(store.clone()),
        delivery: Arc::new(moa_messaging::ProviderDeliverySink::empty(
            "edge-tests@example.invalid",
        )),
        proxy: Arc::new(
            OrchestratorProxy::new(upstream).expect("proxy URL should be syntactically valid"),
        ),
        connector_credentials: Arc::new(
            moa_edge::connector_credential_proxy::ConnectorCredentialProxy::new(upstream)
                .expect("credential proxy URL should be syntactically valid"),
        ),
        clickhouse_lineage: None,
        clickhouse_analytics: None,
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

async fn seed_oauth_access_token(
    pool: &PgPool,
    tenant_id: TenantId,
    token: &str,
    scopes: &[&str],
    resource: &str,
) {
    let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
    let refresh_hash = hex::encode(Sha256::digest(format!("{token}:refresh").as_bytes()));
    let scopes = scopes
        .iter()
        .map(|scope| (*scope).to_string())
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO oauth_tokens (
            tenant_id, client_id, subject_id, subject_type, scopes, resource,
            access_token_hash, access_token_expires_at,
            refresh_token_hash, refresh_token_expires_at
        )
        VALUES ($1, $2, $3, 'operator', $4, $5, $6, NOW() + INTERVAL '1 hour',
                $7, NOW() + INTERVAL '2 hours')
        "#,
    )
    .bind(tenant_id.0)
    .bind(EDGE_OAUTH_CLIENT_ID)
    .bind(Uuid::now_v7())
    .bind(scopes)
    .bind(resource)
    .bind(token_hash)
    .bind(refresh_hash)
    .execute(pool)
    .await
    .expect("seed OAuth access token");
}

async fn post_mcp_with_bearer(
    client: &reqwest::Client,
    edge: &EdgeServer,
    path: &str,
    token: &str,
    mut body: Value,
) -> reqwest::Response {
    let params = body
        .as_object_mut()
        .expect("MCP request must be an object")
        .entry("params")
        .or_insert_with(|| json!({}));
    params
        .as_object_mut()
        .expect("MCP params must be an object")
        .insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "moa-edge-oauth-test",
                    "version": "1",
                },
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
    let method = body["method"]
        .as_str()
        .expect("MCP request method")
        .to_string();
    let name = body.pointer("/params/name").and_then(Value::as_str);
    let mut request = client
        .post(format!("{}{path}", edge.base_url))
        .header("Host", "localhost:10000")
        .header("Origin", "http://localhost:10000")
        .header("Authorization", format!("Bearer {token}"))
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(name) = name {
        request = request.header("Mcp-Name", name);
    }
    request.send().await.expect("send OAuth MCP request")
}

async fn start_purge_upstream() -> PurgeUpstream {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            seen.lock().await.push(request.uri().path().to_string());
            StatusCode::ACCEPTED
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind purge upstream");
    let addr = listener.local_addr().expect("read purge upstream addr");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve purge upstream");
    });
    PurgeUpstream {
        base_url: format!("http://{addr}"),
        requests,
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
        agent_context: Some(AgentContext::system_default()),
        ..SessionMeta::default()
    }
}

fn session_meta_with_id(tenant_id: TenantId, session_id: SessionId) -> SessionMeta {
    SessionMeta {
        id: session_id,
        ..session_meta(tenant_id)
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

#[tokio::test]
async fn tenant_purge_denial_never_dispatches_workflow_db() {
    // Pins: destructive purge authorization is completed at the edge before any Restate dispatch.
    let _guard = DASHBOARD_SESSIONS_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let fga = start_fga_mock(false).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;

    let response = reqwest::Client::new()
        .delete(format!("{}/v1/tenant", edge.base_url))
        .send()
        .await
        .expect("send denied tenant purge");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("admin"));
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{tenant_id}"))
    );

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn oauth_bearers_are_mcp_only_resource_bound_scoped_and_fga_checked_db() {
    // Pins: OAuth access tokens are rejected from REST, require the exact MCP
    // resource and annotation-derived scope, and still pass OpenFGA before MCP
    // dispatch. Mutation check: moving OpenFGA before resource/scope checks adds
    // checks for the first three denied requests; weakening any exact check lets
    // its request reach JSON-RPC dispatch.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;

    let wrong_resource_token = "moa_oauth_at_wrong_resource";
    let read_token = "moa_oauth_at_read_only";
    let write_token = "moa_oauth_at_write_only";
    seed_oauth_access_token(
        store.pool(),
        tenant_id,
        wrong_resource_token,
        &["mcp:read"],
        "https://other.example/mcp",
    )
    .await;
    seed_oauth_access_token(
        store.pool(),
        tenant_id,
        read_token,
        &["mcp:read"],
        EDGE_OAUTH_RESOURCE,
    )
    .await;
    seed_oauth_access_token(
        store.pool(),
        tenant_id,
        write_token,
        &["mcp:write"],
        EDGE_OAUTH_RESOURCE,
    )
    .await;

    let client = reqwest::Client::new();
    let authorization_metadata: Value = client
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            edge.base_url
        ))
        .send()
        .await
        .expect("fetch authorization-server metadata")
        .error_for_status()
        .expect("authorization-server metadata succeeds")
        .json()
        .await
        .expect("decode authorization-server metadata");
    assert_eq!(authorization_metadata["issuer"], json!("https://moa.test"));
    assert_eq!(
        authorization_metadata["authorization_endpoint"],
        json!("https://moa.test/oauth/authorize")
    );
    assert_eq!(
        authorization_metadata["token_endpoint"],
        json!("https://moa.test/oauth/token")
    );
    let resource_metadata: Value = client
        .get(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            edge.base_url
        ))
        .send()
        .await
        .expect("fetch protected-resource metadata")
        .error_for_status()
        .expect("protected-resource metadata succeeds")
        .json()
        .await
        .expect("decode protected-resource metadata");
    assert_eq!(resource_metadata["resource"], json!(EDGE_OAUTH_RESOURCE));
    assert_eq!(
        resource_metadata["authorization_servers"],
        json!(["https://moa.test"])
    );

    let read_call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "sessions_list", "arguments": {} }
    });
    let write_call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "artifact_publish", "arguments": {} }
    });

    let wrong_resource = post_mcp_with_bearer(
        &client,
        &edge,
        "/mcp",
        wrong_resource_token,
        read_call.clone(),
    )
    .await;
    assert_eq!(wrong_resource.status(), StatusCode::FORBIDDEN);

    let read_cannot_write =
        post_mcp_with_bearer(&client, &edge, "/mcp", read_token, write_call.clone()).await;
    assert_eq!(read_cannot_write.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        read_cannot_write
            .headers()
            .get("WWW-Authenticate")
            .and_then(|value| value.to_str().ok()),
        Some(
            "Bearer error=\"insufficient_scope\", scope=\"mcp:write\", resource_metadata=\"https://moa.test/.well-known/oauth-protected-resource/mcp\""
        )
    );

    let write_cannot_read =
        post_mcp_with_bearer(&client, &edge, "/mcp", write_token, read_call.clone()).await;
    assert_eq!(write_cannot_read.status(), StatusCode::FORBIDDEN);
    assert!(
        fga.checks.lock().await.is_empty(),
        "resource and scope denials happen before OpenFGA"
    );

    for path in ["/mcp/", "/mcp/tools"] {
        let noncanonical =
            post_mcp_with_bearer(&client, &edge, path, read_token, read_call.clone()).await;
        assert_eq!(
            noncanonical.status(),
            StatusCode::NOT_FOUND,
            "OAuth bearer must not reach MCP through noncanonical path {path}"
        );
    }
    assert!(
        fga.checks.lock().await.is_empty(),
        "noncanonical MCP paths are rejected before OpenFGA"
    );

    let read_allowed = post_mcp_with_bearer(&client, &edge, "/mcp", read_token, read_call).await;
    assert_eq!(read_allowed.status(), StatusCode::OK);
    let write_allowed = post_mcp_with_bearer(&client, &edge, "/mcp", write_token, write_call).await;
    assert_eq!(write_allowed.status(), StatusCode::OK);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|check| {
        check["tuple_key"]["relation"] == json!("operator")
            && check["tuple_key"]["object"] == json!(format!("tenant:{tenant_id}"))
    }));

    let rest = client
        .post(format!("{}/v1/memory/search", edge.base_url))
        .header("Authorization", format!("Bearer {read_token}"))
        .json(&json!({ "query": "auth" }))
        .send()
        .await
        .expect("send REST request with OAuth bearer");
    assert_eq!(rest.status(), StatusCode::UNAUTHORIZED);

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn repeated_tenant_purge_requests_dispatch_one_stable_workflow_key_db() {
    // Pins: duplicate DELETE requests return the same operation id and target the same tenant-keyed workflow.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'purge dispatch')")
        .bind(tenant_id.0)
        .bind(format!("purge-{tenant_id}"))
        .execute(store.pool())
        .await
        .expect("insert purge dispatch tenant");
    let fga = start_fga_mock(true).await;
    let upstream = start_purge_upstream().await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
        &upstream.base_url,
    )
    .await;
    let client = reqwest::Client::new();

    let first = client
        .delete(format!("{}/v1/tenant", edge.base_url))
        .send()
        .await
        .expect("send first tenant purge");
    let second = client
        .delete(format!("{}/v1/tenant", edge.base_url))
        .send()
        .await
        .expect("send duplicate tenant purge");
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let first: TenantPurgeStatusResponse = first.json().await.expect("decode first purge response");
    let second: TenantPurgeStatusResponse = second
        .json()
        .await
        .expect("decode duplicate purge response");
    assert_eq!(first, second);
    assert_eq!(first.status, TenantPurgeStatus::Pending);

    let requests = upstream.requests.lock().await.clone();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|path| path == &format!("/restate/send/TenantPurge/{tenant_id}/run"))
    );

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn tenant_purge_status_uses_workspace_admin_after_tenant_tuple_is_gone_db() {
    // Pins: post-delete status falls back to canonical workspace admin because tenant-local credentials and tuples are purged.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let caller_tenant_id = TenantId::new();
    let workspace_object = format!("workspace:{}", moa_core::WORKSPACE_ID);
    let fga = start_fga_mock_with_objects(false, vec![workspace_object.clone()]).await;
    let upstream = start_purge_upstream().await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, caller_tenant_id),
        Some(fga.client.clone()),
        &upstream.base_url,
    )
    .await;

    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/tenant/purge/tenant-purge-{tenant_id}",
            edge.base_url
        ))
        .send()
        .await
        .expect("send workspace-admin purge status request");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 2);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{tenant_id}"))
    );
    assert_eq!(checks[1]["tuple_key"]["object"], json!(workspace_object));
    assert_eq!(checks[1]["tuple_key"]["relation"], json!("admin"));

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
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
                assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
            },
        )
        .await
        .expect("emit tool result");
}

async fn set_session_updated_at(
    store: &PostgresSessionStore,
    schema_name: &str,
    session_id: SessionId,
    updated_at: chrono::DateTime<Utc>,
) {
    let schema_name = schema_name.replace('"', "\"\"");
    sqlx::query(&format!(
        r#"UPDATE "{schema_name}".sessions SET updated_at = $1 WHERE id = $2"#
    ))
    .bind(updated_at)
    .bind(session_id.0)
    .execute(store.pool())
    .await
    .expect("set session updated_at fixture");
}

async fn emit_user_message(store: &PostgresSessionStore, session_id: SessionId, text: &str) {
    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: text.to_string(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("emit dashboard user message");
}

fn json_uuid(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(
        value
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{field} should be a UUID string in {value}")),
    )
    .expect("field should parse as UUID")
}

#[tokio::test]
async fn dashboard_sessions_operator_reads_list_detail_and_redacted_events_db() {
    // Pins: tenant operators can read session list/detail/events through HTTP with
    // opaque cursors and without raw event payload leakage.
    let _guard = DASHBOARD_SESSIONS_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let latest_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000202));
    let older_id = SessionId(Uuid::from_u128(0x00000000000000000000000000000101));
    let latest_time = chrono::DateTime::parse_from_rfc3339("2026-07-05T12:00:00Z")
        .expect("fixture timestamp should parse")
        .with_timezone(&Utc);
    let older_time = chrono::DateTime::parse_from_rfc3339("2026-07-05T11:59:00Z")
        .expect("fixture timestamp should parse")
        .with_timezone(&Utc);

    store
        .create_session(session_meta_with_id(tenant_id, latest_id))
        .await
        .expect("create latest dashboard session");
    store
        .create_session(session_meta_with_id(tenant_id, older_id))
        .await
        .expect("create older dashboard session");
    set_session_updated_at(&store, &schema_name, latest_id, latest_time).await;
    set_session_updated_at(&store, &schema_name, older_id, older_time).await;
    emit_user_message(&store, latest_id, "raw-dashboard-secret-one").await;
    emit_user_message(&store, latest_id, "raw-dashboard-secret-two").await;

    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let client = reqwest::Client::new();

    let first_list: Value = client
        .get(format!("{}/v1/dashboard/sessions?limit=1", edge.base_url))
        .send()
        .await
        .expect("send first dashboard list request")
        .error_for_status()
        .expect("first dashboard list should succeed")
        .json()
        .await
        .expect("decode first dashboard list");
    let first_sessions = first_list["sessions"]
        .as_array()
        .expect("sessions should be an array");
    assert_eq!(first_sessions.len(), 1);
    assert_eq!(json_uuid(&first_sessions[0], "session_id"), latest_id.0);
    let list_cursor = first_list["next_cursor"]
        .as_str()
        .expect("first page should return an opaque cursor")
        .to_string();
    assert!(
        !list_cursor.contains(&latest_id.to_string()),
        "HTTP cursor should not expose the raw session id"
    );

    let second_list: Value = client
        .get(format!(
            "{}/v1/dashboard/sessions?limit=1&cursor={list_cursor}",
            edge.base_url
        ))
        .send()
        .await
        .expect("send second dashboard list request")
        .error_for_status()
        .expect("second dashboard list should succeed")
        .json()
        .await
        .expect("decode second dashboard list");
    let second_sessions = second_list["sessions"]
        .as_array()
        .expect("sessions should be an array");
    assert_eq!(second_sessions.len(), 1);
    assert_eq!(json_uuid(&second_sessions[0], "session_id"), older_id.0);
    assert_eq!(second_list["next_cursor"], Value::Null);

    let detail: Value = client
        .get(format!(
            "{}/v1/dashboard/sessions/{}",
            edge.base_url, latest_id
        ))
        .send()
        .await
        .expect("send dashboard detail request")
        .error_for_status()
        .expect("dashboard detail should succeed")
        .json()
        .await
        .expect("decode dashboard detail");
    assert_eq!(json_uuid(&detail, "session_id"), latest_id.0);
    assert_eq!(json_uuid(&detail, "tenant_id"), tenant_id.0);
    assert_eq!(detail["event_count"], json!(2));

    let first_events: Value = client
        .get(format!(
            "{}/v1/dashboard/sessions/{}/events?limit=1",
            edge.base_url, latest_id
        ))
        .send()
        .await
        .expect("send first dashboard events request")
        .error_for_status()
        .expect("first dashboard events page should succeed")
        .json()
        .await
        .expect("decode first dashboard events page");
    let first_events_array = first_events["events"]
        .as_array()
        .expect("events should be an array");
    assert_eq!(first_events_array.len(), 1);
    assert_eq!(first_events_array[0]["sequence_num"], json!(0));
    assert_eq!(
        first_events_array[0]["summary"],
        json!("user message with 0 attachments")
    );
    let first_events_json =
        serde_json::to_string(&first_events).expect("event page should serialize");
    assert!(
        !first_events_json.contains("raw-dashboard-secret"),
        "dashboard event responses must not leak raw payload text: {first_events_json}"
    );
    let event_cursor = first_events["next_cursor"]
        .as_str()
        .expect("first event page should return an opaque cursor")
        .to_string();

    let second_events: Value = client
        .get(format!(
            "{}/v1/dashboard/sessions/{}/events?limit=1&cursor={event_cursor}",
            edge.base_url, latest_id
        ))
        .send()
        .await
        .expect("send second dashboard events request")
        .error_for_status()
        .expect("second dashboard events page should succeed")
        .json()
        .await
        .expect("decode second dashboard events page");
    let second_events_array = second_events["events"]
        .as_array()
        .expect("events should be an array");
    assert_eq!(second_events_array.len(), 1);
    assert_eq!(second_events_array[0]["sequence_num"], json!(1));
    assert_eq!(second_events["next_cursor"], Value::Null);

    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert!(checks.iter().all(|check| {
        check["tuple_key"]["object"] == json!(format!("tenant:{tenant_id}"))
            && check["tuple_key"]["relation"] == json!("operator")
    }));

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn dashboard_sessions_cross_tenant_read_is_denied_db() {
    // Pins: passing another tenant_id does not bypass tenant operator authz.
    let _guard = DASHBOARD_SESSIONS_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = create_test_store().await;
    let caller_tenant = TenantId::new();
    let target_tenant = TenantId::new();
    let target_session = store
        .create_session(session_meta(target_tenant))
        .await
        .expect("create target tenant session");
    let fga = start_fga_mock(false).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, caller_tenant),
        Some(fga.client.clone()),
    )
    .await;

    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/dashboard/sessions/{}?tenant_id={target_tenant}",
            edge.base_url, target_session
        ))
        .send()
        .await
        .expect("send cross-tenant dashboard detail request");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert!(checks.iter().all(|check| {
        check["tuple_key"]["object"] == json!(format!("tenant:{target_tenant}"))
            && check["tuple_key"]["relation"] == json!("operator")
    }));

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn dashboard_sessions_workspace_admin_reads_explicit_tenant_db() {
    // Pins: workspace-admin inheritance reaches cross-tenant dashboard reads only
    // when the target tenant is supplied explicitly at the HTTP boundary.
    let _guard = DASHBOARD_SESSIONS_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = create_test_store().await;
    let caller_tenant = TenantId::new();
    let target_tenant = TenantId::new();
    let target_session = store
        .create_session(session_meta(target_tenant))
        .await
        .expect("create target tenant session");
    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, caller_tenant),
        Some(fga.client.clone()),
    )
    .await;

    let response: Value = reqwest::Client::new()
        .get(format!(
            "{}/v1/dashboard/sessions?tenant_id={target_tenant}",
            edge.base_url
        ))
        .send()
        .await
        .expect("send explicit-tenant dashboard list request")
        .error_for_status()
        .expect("explicit tenant dashboard list should succeed")
        .json()
        .await
        .expect("decode explicit tenant dashboard list");

    let sessions = response["sessions"]
        .as_array()
        .expect("sessions should be an array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(json_uuid(&sessions[0], "session_id"), target_session.0);
    assert_eq!(json_uuid(&sessions[0], "tenant_id"), target_tenant.0);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert!(checks.iter().all(|check| {
        check["tuple_key"]["object"] == json!(format!("tenant:{target_tenant}"))
            && check["tuple_key"]["relation"] == json!("operator")
    }));

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn dashboard_sessions_malformed_cursor_is_rejected_db() {
    // Pins: dashboard cursor strings are opaque HTTP tokens and malformed tokens
    // are rejected before reaching the read model.
    let _guard = DASHBOARD_SESSIONS_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    store
        .create_session(session_meta(tenant_id))
        .await
        .expect("create dashboard session");
    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;

    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/dashboard/sessions?cursor=not-a-valid-cursor",
            edge.base_url
        ))
        .send()
        .await
        .expect("send malformed cursor request");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

fn string_cell(cell: &AnalyticsCell) -> &str {
    match cell {
        AnalyticsCell::String(value) => value.as_str(),
        other => panic!("expected string cell, got {other:?}"),
    }
}

fn i64_cell(cell: &AnalyticsCell) -> i64 {
    match cell {
        AnalyticsCell::Number(value) => value.as_i64().expect("integer cell"),
        other => panic!("expected integer cell, got {other:?}"),
    }
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
    .bind(chrono::DateTime::<chrono::Utc>::from_timestamp_micros(chrono::Utc::now().timestamp_micros()).expect("microsecond timestamp"))
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
async fn authz_gate_runs_before_analytics_query_db() {
    // Pins: protected direct reads fail closed at authz before reading session data.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        None,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/analytics/query", edge.base_url))
        .json(&json!({
            "dataset": "sessions",
            "measures": [{ "aggregation": "count", "alias": "sessions" }]
        }))
        .send()
        .await
        .expect("send analytics query request");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    stop_server(edge.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn analytics_query_uses_requested_tenant_when_authorized_db() {
    // Pins: dashboard analytics queries can intentionally target another tenant,
    // but only after an operator authz check against that target tenant.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    seed_tool_call(&store, tenant_a, "tenant_a_tool").await;
    seed_tool_call(&store, tenant_b, "tenant_b_tool").await;
    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh analytics views");

    let fga = start_fga_mock(true).await;
    let user_edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_a),
        Some(fga.client.clone()),
    )
    .await;
    let user_response: AnalyticsQueryResponse = reqwest::Client::new()
        .post(format!("{}/v1/analytics/query", user_edge.base_url))
        .json(&json!({
            "dataset": "tool_calls",
            "tenant_id": tenant_b,
            "dimensions": [{ "field": "tool_name" }],
            "measures": [{ "aggregation": "count", "alias": "calls" }],
            "filters": [{
                "field": "called_at",
                "operator": "gte",
                "value": (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339()
            }],
            "order_by": [{ "field": "calls", "direction": "desc" }],
            "limit": 10
        }))
        .send()
        .await
        .expect("send user analytics query")
        .error_for_status()
        .expect("user analytics query should succeed")
        .json()
        .await
        .expect("decode user analytics response");

    assert_eq!(user_response.metadata.effective_tenant_id, Some(tenant_b));
    assert_eq!(user_response.rows.len(), 1);
    assert_eq!(string_cell(&user_response.rows[0][0]), "tenant_b_tool");
    assert_eq!(i64_cell(&user_response.rows[0][1]), 1);

    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{tenant_b}"))
    );
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("operator"));

    stop_server(user_edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn analytics_query_denies_unauthorized_requested_tenant_db() {
    // Pins: a requested tenant_id does not bypass tenant operator authz.
    let (store, database_url, schema_name) = create_test_store().await;
    let caller_tenant = TenantId::new();
    let target_tenant = TenantId::new();
    seed_tool_call(&store, target_tenant, "hidden_tool").await;
    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh analytics views");

    let fga = start_fga_mock(false).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, caller_tenant),
        Some(fga.client.clone()),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/analytics/query", edge.base_url))
        .json(&json!({
            "dataset": "tool_calls",
            "tenant_id": target_tenant,
            "dimensions": [{ "field": "tool_name" }],
            "measures": [{ "aggregation": "count", "alias": "calls" }]
        }))
        .send()
        .await
        .expect("send unauthorized target analytics query");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{target_tenant}"))
    );
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("operator"));

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn analytics_catalog_requires_tenant_operator_db() {
    // Pins: catalog is part of the tenant operator surface and stays behind edge authz.
    let (store, database_url, schema_name) = create_test_store().await;
    let caller_tenant = TenantId::new();
    let target_tenant = TenantId::new();
    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, caller_tenant),
        Some(fga.client.clone()),
    )
    .await;

    let response: AnalyticsCatalogResponse = reqwest::Client::new()
        .get(format!(
            "{}/v1/analytics/catalog?tenant_id={target_tenant}",
            edge.base_url
        ))
        .send()
        .await
        .expect("send analytics catalog request")
        .error_for_status()
        .expect("catalog should succeed")
        .json()
        .await
        .expect("decode analytics catalog");

    assert!(
        response
            .datasets
            .iter()
            .any(|dataset| dataset.id == "tool_calls")
    );
    assert!(
        response
            .datasets
            .iter()
            .any(|dataset| dataset.id == "events")
    );

    let checks = fga.checks.lock().await.clone();
    assert_eq!(checks.len(), 1);
    assert_eq!(
        checks[0]["tuple_key"]["object"],
        json!(format!("tenant:{target_tenant}"))
    );
    assert_eq!(checks[0]["tuple_key"]["relation"], json!("operator"));

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn analytics_query_uses_configured_schema_db() {
    // Pins: direct analytics query reads read models built from the configured schema, not the default schema.
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
    store
        .refresh_analytics_materialized_views()
        .await
        .expect("refresh analytics views");

    let fga = start_fga_mock(true).await;
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let response: AnalyticsQueryResponse = reqwest::Client::new()
        .post(format!("{}/v1/analytics/query", edge.base_url))
        .json(&json!({
            "dataset": "events",
            "dimensions": [{ "field": "event_type" }, { "field": "session_id" }],
            "measures": [{ "aggregation": "count", "alias": "events" }],
            "filters": [{
                "field": "occurred_at",
                "operator": "gte",
                "value": (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339()
            }],
            "limit": 5
        }))
        .send()
        .await
        .expect("send analytics query request")
        .error_for_status()
        .expect("analytics query should succeed")
        .json()
        .await
        .expect("decode analytics query response");

    assert_eq!(response.metadata.effective_tenant_id, Some(tenant_id));
    assert_eq!(response.rows.len(), 1);
    assert_eq!(string_cell(&response.rows[0][0]), "UserMessage");
    assert_eq!(string_cell(&response.rows[0][1]), session_id.to_string());
    assert_eq!(i64_cell(&response.rows[0][2]), 1);

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
        identity(IdentityType::Operator, tenant_a),
        Some(fga.client.clone()),
    )
    .await;

    let response: LineageQueryResponse = reqwest::Client::new()
        .post(format!("{}/v1/lineage/query", edge.base_url))
        .json(&json!({
            "filters": {
                "record_kind": 7,
                "from_time": (chrono::DateTime::<chrono::Utc>::from_timestamp_micros(chrono::Utc::now().timestamp_micros()).expect("microsecond timestamp") - Duration::minutes(5)).to_rfc3339()
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

#[tokio::test]
async fn the_github_secret_scanning_placeholder_route_is_not_registered_db() {
    // Pins: the edge ladder advertises no GitHub secret-scanning partner
    // endpoint. It used to register one that answered every request with 501,
    // which is worse than absence: a caller could not tell "we registered this
    // and never built it" from "you have the wrong URL", and the path appeared
    // in the public ladder as though it were part of the API. The regex half of
    // the partner contract is unaffected and stays pinned by
    // `github_secret_scanning_regex_is_public_contract` in moa-auth/providers.
    //
    // The `/healthz` leg is a negative control: without it, a 404 from a router
    // that failed to start would read exactly like a route that was removed.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        None,
    )
    .await;
    let client = reqwest::Client::new();

    let live = client
        .get(format!("{}/healthz", edge.base_url))
        .send()
        .await
        .expect("send edge health probe");
    assert_eq!(
        live.status(),
        StatusCode::OK,
        "the edge router must be serving before an absent-route assertion means anything"
    );

    let response = client
        .post(format!(
            "{}/v1/security/secret-scanning/github",
            edge.base_url
        ))
        .json(&json!({"token": "moa_live_placeholder"}))
        .send()
        .await
        .expect("send retired secret-scanning request");

    let status = response.status();
    let reason = response
        .headers()
        .get("x-moa-reason")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the retired secret-scanning path must answer like any unknown path; \
         observed status {status} with x-moa-reason {reason:?}"
    );
    assert!(
        reason.is_none(),
        "a retired route must not still be announcing why it is unimplemented; observed {reason:?}"
    );

    stop_server(edge.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn disabled_connector_management_is_dark_before_auth_body_and_proxy_db() {
    // Pins: the rollout switch makes both ordinary management and private
    // credential proxy routes indistinguishable from absent routes before any
    // caller authentication, JSON/header validation, or upstream request.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let auth_calls = Arc::new(AtomicUsize::new(0));
    let upstream = start_purge_upstream().await;
    let edge = start_edge_with_auth_upstream_and_connector_management(
        &store,
        &database_url,
        &schema_name,
        Arc::new(CountingAuth {
            identity: identity(IdentityType::Operator, tenant_id),
            calls: auth_calls.clone(),
        }),
        None,
        &upstream.base_url,
        false,
    )
    .await;
    let client = reqwest::Client::new();
    let connection_id = Uuid::now_v7();

    let routes = [
        (
            RequestMethod::POST,
            "/v1/connectors/connections".to_string(),
        ),
        (RequestMethod::GET, "/v1/connectors/connections".to_string()),
        (
            RequestMethod::GET,
            format!("/v1/connectors/connections/{connection_id}"),
        ),
        (
            RequestMethod::DELETE,
            format!("/v1/connectors/connections/{connection_id}"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/verify"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/activate"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/suspend"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/resume"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/disconnect"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/delete"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/use/grant"),
        ),
        (
            RequestMethod::POST,
            format!("/v1/connectors/connections/{connection_id}/use/revoke"),
        ),
        (
            RequestMethod::PUT,
            format!("/v1/connectors/connections/{connection_id}/credentials/primary"),
        ),
    ];
    for (method, path) in routes {
        let response = client
            .request(method, format!("{}{path}", edge.base_url))
            .body("{")
            .send()
            .await
            .expect("send disabled connector management request");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "disabled connector route {path} must stay dark"
        );
    }
    assert_eq!(
        auth_calls.load(Ordering::SeqCst),
        0,
        "dark connector routes must not authenticate callers"
    );
    assert!(
        upstream.requests.lock().await.is_empty(),
        "dark connector routes must not reach either orchestrator ingress"
    );

    stop_server(edge.server).await;
    stop_server(upstream.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn auth0_connection_linking_webhook_is_not_exposed_db() {
    // Pins: Auth0 remains an identity provider, but its removed token-vault
    // connection-linking webhook is not part of the public edge contract.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let edge = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        None,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/webhooks/auth0/connection-linked",
            edge.base_url
        ))
        .json(&json!({"user_id": Uuid::now_v7()}))
        .send()
        .await
        .expect("send auth0 connection-linked webhook");

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "the deleted token-vault webhook must stay absent"
    );

    stop_server(edge.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}
