//! DB-backed transport and forwarding coverage for tenant-operations MCP.

use super::*;
use axum::body::to_bytes;
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN};
use axum::response::IntoResponse;

struct RejectAuth;

#[async_trait]
impl AuthProvider for RejectAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Err(AuthError::Rejected)
    }

    fn name(&self) -> &'static str {
        "reject-test"
    }
}

async fn post_mcp(client: &reqwest::Client, edge: &EdgeServer, body: Value) -> reqwest::Response {
    client
        .post(format!("{}/mcp", edge.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .expect("send MCP request")
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "moa-edge-test", "version": "1" }
        }
    })
}

#[tokio::test]
async fn mcp_authentication_authorization_host_and_origin_fail_closed_db() {
    // Pins: every MCP message crosses HTTP auth/authz and exact Host/Origin checks before JSON-RPC dispatch.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let fga = start_fga_mock(true).await;

    let rejected = start_edge_with_auth_and_upstream(
        &store,
        &database_url,
        &schema_name,
        Arc::new(RejectAuth),
        Some(fga.client.clone()),
        "http://127.0.0.1:1",
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/mcp", rejected.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header(AUTHORIZATION, "Bearer rejected")
        .json(&initialize_request())
        .send()
        .await
        .expect("send rejected MCP request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    stop_server(rejected.server).await;

    let contact = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let response = post_mcp(&reqwest::Client::new(), &contact, initialize_request()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    stop_server(contact.server).await;

    let operator = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/mcp", operator.base_url))
        .header(HOST, "evil.example.com")
        .header(ORIGIN, "https://evil.example.com")
        .json(&initialize_request())
        .send()
        .await
        .expect("send disallowed origin request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = post_mcp(&reqwest::Client::new(), &operator, initialize_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("decode initialize response");
    assert_eq!(body["result"]["serverInfo"]["name"], json!("moa-edge"));

    stop_server(operator.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn mcp_artifact_validate_forwards_typed_tenant_request_and_strips_credentials_db() {
    // Pins: representative MCP commands use an allowlisted Restate path, inject tenant scope, and never forward caller credentials.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let captured = Arc::new(Mutex::new(None::<(String, HeaderMap, Value)>));
    let seen = captured.clone();
    let upstream_app = Router::new().fallback(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            let path = request.uri().path().to_string();
            let headers = request.headers().clone();
            let body = to_bytes(request.into_body(), 64 * 1024)
                .await
                .expect("read forwarded body");
            let body: Value = serde_json::from_slice(&body).expect("decode forwarded body");
            *seen.lock().await = Some((path, headers, body));
            axum::Json(json!({ "valid": true, "validation_report": { "errors": [] } }))
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind MCP upstream");
    let upstream_addr = listener.local_addr().expect("read MCP upstream address");
    let upstream_server = tokio::spawn(async move {
        axum::serve(listener, upstream_app)
            .await
            .expect("serve MCP upstream");
    });
    let fga = start_fga_mock(true).await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
        &format!("http://{upstream_addr}"),
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/mcp", edge.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header(AUTHORIZATION, "Bearer caller-secret")
        .header("MCP-Protocol-Version", "2025-06-18")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "artifact_validate",
                "arguments": {
                    "source_format": "json",
                    "source_text": "{\"kind\":\"skill\"}",
                    "status": "draft"
                }
            }
        }))
        .send()
        .await
        .expect("call artifact_validate through MCP");
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.expect("decode MCP tool result");
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(
        response["result"]["structuredContent"]["data"]["valid"],
        json!(true)
    );

    let (path, headers, body) = captured
        .lock()
        .await
        .clone()
        .expect("upstream should receive MCP command");
    assert_eq!(path, "/restate/call/Artifacts/validate");
    assert_eq!(body["tenant_id"], json!(tenant_id));
    assert_eq!(body["source_format"], json!("json"));
    assert!(headers.get(AUTHORIZATION).is_none());
    let tenant_header = tenant_id.to_string();
    assert_eq!(
        headers
            .get("x-moa-tenant-id")
            .and_then(|value| value.to_str().ok()),
        Some(tenant_header.as_str())
    );

    stop_server(edge.server).await;
    stop_server(upstream_server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn mcp_agent_principals_list_forwards_truly_empty_request_db() {
    // Pins: no-body MCP commands remove the caller's JSON entity headers before
    // Restate validation while preserving trusted identity and typed responses.
    let (store, database_url, schema_name) = create_test_store().await;
    let tenant_id = TenantId::new();
    let agent_id = Uuid::now_v7();
    let captured = Arc::new(Mutex::new(None::<(String, HeaderMap, Vec<u8>)>));
    let seen = captured.clone();
    let upstream_app = Router::new().fallback(move |request: axum::extract::Request| {
        let seen = seen.clone();
        async move {
            let path = request.uri().path().to_string();
            let headers = request.headers().clone();
            let body = to_bytes(request.into_body(), 1024)
                .await
                .expect("read forwarded empty body")
                .to_vec();
            let valid = body.is_empty() && headers.get(CONTENT_TYPE).is_none();
            *seen.lock().await = Some((path, headers, body));
            if !valid {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(
                        json!({"message": "expected an empty request without content-type"}),
                    ),
                )
                    .into_response();
            }
            axum::Json(json!([{
                "id": agent_id,
                "tenant_id": tenant_id,
                "operator_user_id": null,
                "display_name": "MCP Principal",
                "status": "active",
                "created_at": "2026-07-11T00:00:00Z",
                "deactivated_at": null,
                "deactivated_reason": null
            }]))
            .into_response()
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind MCP empty-command upstream");
    let upstream_addr = listener
        .local_addr()
        .expect("read MCP empty-command upstream address");
    let upstream_server = tokio::spawn(async move {
        axum::serve(listener, upstream_app)
            .await
            .expect("serve MCP empty-command upstream");
    });
    let fga = start_fga_mock(true).await;
    let edge = start_edge_with_upstream(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Operator, tenant_id),
        Some(fga.client.clone()),
        &format!("http://{upstream_addr}"),
    )
    .await;

    let response = post_mcp(
        &reqwest::Client::new(),
        &edge,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "agent_principals_list", "arguments": {}}
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response
        .json()
        .await
        .expect("decode MCP principal list result");
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(
        response["result"]["structuredContent"]["data"][0]["id"],
        json!(agent_id)
    );

    let (path, headers, body) = captured
        .lock()
        .await
        .clone()
        .expect("upstream should receive empty MCP command");
    assert_eq!(path, "/restate/call/Agents/list");
    assert!(body.is_empty());
    assert!(headers.get(CONTENT_TYPE).is_none());
    assert!(
        headers.get(CONTENT_LENGTH).is_none_or(|value| value == "0"),
        "reqwest may recalculate an empty request only as content-length zero"
    );
    let tenant_header = tenant_id.to_string();
    assert_eq!(
        headers
            .get("x-moa-tenant-id")
            .and_then(|value| value.to_str().ok()),
        Some(tenant_header.as_str())
    );

    stop_server(edge.server).await;
    stop_server(upstream_server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}

#[tokio::test]
async fn mcp_invalid_tool_arguments_return_structured_error_envelope_db() {
    // Pins: schema-violating tool arguments produce the documented isError
    // {error} structuredContent with the serde detail, not a bare text block.
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

    let response = post_mcp(
        &reqwest::Client::new(),
        &edge,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "sessions_list",
                "arguments": { "limit": "abc" }
            }
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.expect("decode invalid-args result");
    assert_eq!(body["result"]["isError"], json!(true));
    let error = body["result"]["structuredContent"]["error"]
        .as_str()
        .expect("errored MCP results must carry the {error} structured envelope");
    assert!(
        error.contains("failed to deserialize parameters"),
        "error message should surface the serde detail, got: {error}"
    );

    stop_server(edge.server).await;
    stop_fga_mock(fga.server).await;
    cleanup_test_store(store, database_url, schema_name).await;
}
