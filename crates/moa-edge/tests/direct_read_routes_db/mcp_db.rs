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
    let mut body = body;
    add_modern_metadata(&mut body);
    let method = body["method"]
        .as_str()
        .expect("MCP request method")
        .to_string();
    let name = body.pointer("/params/name").and_then(Value::as_str);
    let mut request = client
        .post(format!("{}/mcp", edge.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", method)
        .header("Accept", "application/json, text/event-stream")
        .json(&body);
    if let Some(name) = name {
        request = request.header("Mcp-Name", name);
    }
    request.send().await.expect("send MCP request")
}

fn add_modern_metadata(body: &mut Value) {
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
                    "name": "moa-edge-test",
                    "version": "1",
                },
                "io.modelcontextprotocol/clientCapabilities": {},
            }),
        );
}

fn discover_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "server/discover",
        "params": {}
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
        .json(&discover_request())
        .send()
        .await
        .expect("send rejected MCP request");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get("WWW-Authenticate")
            .and_then(|value| value.to_str().ok()),
        Some(
            "Bearer resource_metadata=\"https://moa.test/.well-known/oauth-protected-resource/mcp\", scope=\"mcp:read mcp:write\""
        )
    );
    stop_server(rejected.server).await;

    let contact = start_edge(
        &store,
        &database_url,
        &schema_name,
        identity(IdentityType::Contact, tenant_id),
        Some(fga.client.clone()),
    )
    .await;
    let response = post_mcp(&reqwest::Client::new(), &contact, discover_request()).await;
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
        .json(&discover_request())
        .send()
        .await
        .expect("send disallowed origin request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = post_mcp(&reqwest::Client::new(), &operator, discover_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("Mcp-Session-Id").is_none());
    let body: Value = response.json().await.expect("decode discovery response");
    assert_eq!(body["result"]["resultType"], json!("complete"));
    assert_eq!(
        body["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("moa-edge")
    );
    assert_eq!(body["result"]["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(body["result"]["capabilities"], json!({ "tools": {} }));
    assert_eq!(body["result"]["ttlMs"], json!(3_600_000));
    assert_eq!(body["result"]["cacheScope"], json!("public"));

    let tools = post_mcp(
        &reqwest::Client::new(),
        &operator,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }),
    )
    .await;
    assert_eq!(tools.status(), StatusCode::OK);
    assert!(tools.headers().get("Mcp-Session-Id").is_none());
    let tools: Value = tools.json().await.expect("decode tool catalog");
    assert_eq!(tools["result"]["resultType"], json!("complete"));
    assert_eq!(tools["result"]["ttlMs"], json!(300_000));
    assert_eq!(tools["result"]["cacheScope"], json!("public"));
    assert_eq!(
        tools["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("moa-edge")
    );
    let names = tools["result"]["tools"]
        .as_array()
        .expect("tool catalog array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert!(names.windows(2).all(|pair| pair[0] < pair[1]));

    let legacy = reqwest::Client::new()
        .post(format!("{}/mcp", operator.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "initialize")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "legacy-init",
            "method": "initialize",
            "params": {
                "protocolVersion": "2026-07-28",
                "capabilities": {},
                "clientInfo": { "name": "legacy", "version": "1" }
            }
        }))
        .send()
        .await
        .expect("send removed initialize request");
    assert_eq!(legacy.status(), StatusCode::NOT_FOUND);
    let legacy: Value = legacy.json().await.expect("decode initialize rejection");
    assert_eq!(legacy["id"], json!("legacy-init"));
    assert_eq!(legacy["error"]["code"], json!(-32601));

    let missing_meta = reqwest::Client::new()
        .post(format!("{}/mcp", operator.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "missing-meta",
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await
        .expect("send request without per-request metadata");
    assert_eq!(missing_meta.status(), StatusCode::BAD_REQUEST);
    let missing_meta: Value = missing_meta
        .json()
        .await
        .expect("decode missing-meta rejection");
    assert_eq!(missing_meta["error"]["code"], json!(-32602));

    let mut missing_header_body = discover_request();
    add_modern_metadata(&mut missing_header_body);
    let missing_header = reqwest::Client::new()
        .post(format!("{}/mcp", operator.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Accept", "application/json, text/event-stream")
        .json(&missing_header_body)
        .send()
        .await
        .expect("send request without Mcp-Method");
    assert_eq!(missing_header.status(), StatusCode::BAD_REQUEST);
    let missing_header: Value = missing_header
        .json()
        .await
        .expect("decode missing-header rejection");
    assert_eq!(missing_header["error"]["code"], json!(-32020));

    let unsupported = reqwest::Client::new()
        .post(format!("{}/mcp", operator.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("Mcp-Method", "tools/list")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": "unsupported-version",
            "method": "tools/list",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2025-11-25",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "old-client",
                        "version": "1"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        }))
        .send()
        .await
        .expect("send unsupported protocol version");
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
    let unsupported: Value = unsupported
        .json()
        .await
        .expect("decode unsupported-version rejection");
    assert_eq!(unsupported["error"]["code"], json!(-32022));
    assert_eq!(
        unsupported["error"]["data"],
        json!({
            "requested": "2025-11-25",
            "supported": ["2026-07-28"]
        })
    );

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

    let mut tool_call = json!({
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
    });
    add_modern_metadata(&mut tool_call);
    let response = reqwest::Client::new()
        .post(format!("{}/mcp", edge.base_url))
        .header(HOST, "localhost:10000")
        .header(ORIGIN, "http://localhost:10000")
        .header(AUTHORIZATION, "Bearer caller-secret")
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/call")
        .header("Mcp-Name", "artifact_validate")
        .header("Accept", "application/json, text/event-stream")
        .json(&tool_call)
        .send()
        .await
        .expect("call artifact_validate through MCP");
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.expect("decode MCP tool result");
    assert_eq!(response["result"]["resultType"], json!("complete"));
    assert_eq!(response["result"]["isError"], json!(false));
    assert_eq!(
        response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("moa-edge")
    );
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
