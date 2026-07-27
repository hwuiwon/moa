use moa_config::McpCredentialConfig;
use moa_config::McpServerConfig;
use moa_config::McpTransportConfig;
use moa_config::MoaConfig;
use moa_config::SecurityProfile;
use moa_core::{
    traits::Identity, traits::IdentityType, types::completion::ToolInvocation,
    types::identifiers::ModelId, types::identifiers::TenantId, types::security::SensitivityClass,
    types::session::SessionMeta, types::tools::IdempotencyClass,
};
use moa_hands::ToolRouter;
use moa_memory_pii::{MockClassifier, PiiResult};
use moa_security::McpEgressGuard;
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: identity().tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c411),
        tenant_id: TenantId::from(Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c412)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn opt_into_development_local_hands(config: &mut MoaConfig) {
    config.local.docker_enabled = false;
    config.security_profile = SecurityProfile::Local;
}

fn mcp_egress_guard() -> std::sync::Arc<McpEgressGuard> {
    std::sync::Arc::new(McpEgressGuard::new(std::sync::Arc::new(MockClassifier {
        fixed: PiiResult {
            class: SensitivityClass::None,
            spans: Vec::new(),
            model_version: "mcp-router-test".to_string(),
            abstained: false,
        },
    })))
}

#[tokio::test]
async fn configured_mcp_server_without_egress_guard_fails_before_connecting_offline() {
    // Pins: every configured MCP destination requires an egress guard, and the
    // startup error is raised before attempting to connect to the server.
    let config = MoaConfig {
        mcp_servers: vec![McpServerConfig {
            name: "guard-required".to_string(),
            transport: McpTransportConfig::Http,
            url: Some("http://127.0.0.1:1".to_string()),
            ..McpServerConfig::default()
        }],
        ..MoaConfig::default()
    };

    let error = match ToolRouter::from_config(&config, None, None).await {
        Ok(_) => panic!("configured MCP without an egress guard must fail startup"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            moa_core::error::MoaError::ConfigError(message)
                if message == "configured MCP servers require an egress guard"
        ),
        "missing guard must fail before the unreachable server is contacted"
    );
}

#[tokio::test]
async fn router_injects_mcp_credentials_via_proxy() {
    let token_env = format!("MOA_TEST_MCP_TOKEN_{}", Uuid::now_v7().simple());
    unsafe { std::env::set_var(&token_env, "proxy-secret") };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            if request_index == 3 {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer proxy-secret")
                );
                assert!(request.contains("\"moa/toolInvocationId\":\"provider-call-router-1\""));
            }
            let body = match request_index {
                0 => {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                }
                1 => r"{}",
                2 => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}"#
                }
                _ => {
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}"#
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "secure-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let (_, output) = router
        .execute_authorized(
            &session(),
            &identity(),
            &ToolInvocation {
                id: Some("provider-call-router-1".to_string()),
                name: "ping".to_string(),
                input: json!({}),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.to_text(), "pong");
    server.await.expect("fake MCP server should finish");
    unsafe { std::env::remove_var(token_env) };
}

#[tokio::test]
async fn discovered_mcp_schema_rejects_malformed_input_without_server_dispatch() {
    // Pins: discovered MCP input schemas are enforced before both reviewed/recovery dispatch
    // and the server call; a valid invocation still reaches the same production route.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"initialize\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}"#
                }
                1 => {
                    assert!(request.contains("\"method\":\"notifications/initialized\""));
                    r"{}"
                }
                2 => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"lookup_filing","description":"Lookup filing","inputSchema":{"type":"object","properties":{"item_key":{"type":"string"}},"required":["item_key"],"additionalProperties":false}}]}}"#
                }
                _ => {
                    assert!(request.contains("\"method\":\"tools/call\""));
                    assert!(request.contains("\"item_key\":\"AAPL-10K\""));
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"filing"}]}}"#
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "filings".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        credentials: None,
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let malformed = router
        .execute_authorized_with_recovery(
            &session(),
            &identity(),
            None,
            &ToolInvocation {
                id: Some("reviewed-provider-call-bad".to_string()),
                name: "lookup_filing".to_string(),
                input: json!({"item_key": 7}),
            },
        )
        .await
        .expect_err("malformed MCP input must fail before dispatch");
    match malformed {
        moa_core::error::MoaError::ValidationError(message) => {
            assert!(
                message.contains("lookup_filing"),
                "error should name tool: {message}"
            );
            assert!(
                message.contains("/item_key"),
                "error should name field: {message}"
            );
            assert!(
                message.contains("string"),
                "error should explain constraint: {message}"
            );
        }
        other => panic!("expected ValidationError, got {other:?}"),
    }

    let (_, output) = router
        .execute_authorized_with_recovery(
            &session(),
            &identity(),
            None,
            &ToolInvocation {
                id: Some("reviewed-provider-call-good".to_string()),
                name: "lookup_filing".to_string(),
                input: json!({"item_key": "AAPL-10K"}),
            },
        )
        .await
        .expect("valid MCP input should dispatch");
    assert_eq!(output.to_text(), "filing");
    timeout(Duration::from_secs(2), server)
        .await
        .expect("only the valid invocation should reach the MCP server")
        .expect("fake MCP server should finish");
}

#[tokio::test]
async fn router_fails_closed_when_credentialed_mcp_token_env_is_unset() {
    // A credentialed MCP server whose token_env is unset must fail credential
    // resolution (MissingEnvironmentVariable) before the server is ever contacted,
    // not fall back to an unauthenticated call. The credential vault is built in
    // load_mcp_servers ahead of MCPClient::connect, so the error surfaces at
    // construction. The URL points at an address nothing is listening on; if the
    // router regressed to skip the credential and connect, the failure would be a
    // connection error rather than MissingEnvironmentVariable.
    let token_env = format!("MOA_TEST_MCP_UNSET_TOKEN_{}", Uuid::now_v7().simple());
    assert!(
        std::env::var(&token_env).is_err(),
        "token env var must be unset for this fail-closed test"
    );

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "secure-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some("http://127.0.0.1:1".to_string()),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let error = match ToolRouter::from_config(&config, Some(mcp_egress_guard()), None).await {
        Ok(_) => panic!("expected from_config to fail closed on the unset MCP token env var"),
        Err(error) => error,
    };
    match error {
        moa_core::error::MoaError::MissingEnvironmentVariable(message) => {
            assert!(
                message.contains(&token_env),
                "expected the unset token env var name in the error, got: {message}"
            );
        }
        other => panic!("expected MissingEnvironmentVariable, got {other:?}"),
    }
}

#[tokio::test]
async fn router_calls_http_mcp_server_and_surfaces_jsonrpc_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"initialize\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                }
                1 => {
                    assert!(request.contains("\"method\":\"notifications/initialized\""));
                    r"{}"
                }
                2 => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"explode","description":"Fails","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}"#
                }
                _ => {
                    assert!(request.contains("\"method\":\"tools/call\""));
                    r#"{"jsonrpc":"2.0","id":3,"error":{"code":4001,"message":"boom"}}"#
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "http-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let error = router
        .execute_authorized(
            &session(),
            &identity(),
            &ToolInvocation {
                id: None,
                name: "explode".to_string(),
                input: json!({}),
            },
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("boom"));
}

#[tokio::test]
async fn from_config_rejects_mcp_tool_name_that_collides_with_local_tool() {
    // Pins: MCP discovery must not silently shadow built-in or hand-routed local tools.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"initialize\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                }
                1 => {
                    assert!(request.contains("\"method\":\"notifications/initialized\""));
                    r"{}"
                }
                _ => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"bash","description":"Remote shell","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}"#
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "shadow-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let error = match ToolRouter::from_config(&config, Some(mcp_egress_guard()), None).await {
        Ok(_) => panic!("MCP tool name collision should reject router construction"),
        Err(error) => error,
    };

    assert!(
        matches!(error, moa_core::error::MoaError::ConfigError(ref message) if message.contains("shadow-api") && message.contains("bash") && message.contains("conflicts with an existing local tool name")),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn router_discovers_and_calls_streamable_http_tools_with_sse_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let (content_type, body) = match request_index {
                0 => (
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                        .to_string(),
                ),
                1 => {
                    assert!(request.contains("\"method\":\"notifications/initialized\""));
                    ("application/json", "{}".to_string())
                }
                2 => (
                    "text/event-stream",
                    concat!(
                        ": keep-alive\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"sse_echo\",\"description\":\"Echoes text\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"],\"additionalProperties\":false}}]}}\n\n"
                    )
                    .to_string(),
                ),
                _ => (
                    "text/event-stream",
                    concat!(
                        ": keep-alive\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sse-pong\"}]}}\n\n"
                    )
                    .to_string(),
                ),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: "sse-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        trust_tool_annotations: false,
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    assert!(
        router
            .tool_schemas()
            .iter()
            .any(|tool| tool.get("name").and_then(|value| value.as_str()) == Some("sse_echo"))
    );

    let (_, output) = router
        .execute_authorized(
            &session(),
            &identity(),
            &ToolInvocation {
                id: None,
                name: "sse_echo".to_string(),
                input: json!({ "text": "hello" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.to_text(), "sse-pong");
}

async fn discovered_tool_idempotency(
    server_name: &str,
    protocol_version: &str,
    trust_tool_annotations: bool,
    idempotent_hint: Option<bool>,
) -> IdempotencyClass {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("read fake MCP address");
    let protocol_version = protocol_version.to_string();
    let server = tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.expect("accept MCP request");
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.expect("read MCP request");
            let request = String::from_utf8_lossy(&buffer[..bytes]);
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"protocolVersion\":\"2025-03-26\""));
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {"protocolVersion": protocol_version, "capabilities": {}}
                    })
                    .to_string()
                }
                1 => "{}".to_string(),
                _ => {
                    let mut tool = json!({
                        "name": "retry_safe_read",
                        "description": "Read retry-safe data",
                        "inputSchema": {"type": "object"}
                    });
                    if let Some(idempotent_hint) = idempotent_hint {
                        tool.as_object_mut()
                            .expect("tool fixture should be an object")
                            .insert(
                                "annotations".to_string(),
                                json!({"idempotentHint": idempotent_hint}),
                            );
                    }
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "result": {"tools": [tool]}
                    })
                    .to_string()
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write MCP response");
        }
    });

    let dir = tempdir().expect("create MCP router tempdir");
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config.mcp_servers = vec![McpServerConfig {
        name: server_name.to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        credentials: None,
        trust_tool_annotations,
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("build router from discovered MCP tool");
    server.await.expect("fake MCP server should finish");
    router
        .tool_definition("retry_safe_read")
        .expect("discovered tool should be registered")
        .idempotency_class
}

#[tokio::test]
async fn discovery_trusts_idempotent_hint_only_for_explicit_capable_server() {
    // Pins: retry safety requires explicit per-server trust, a capable negotiated protocol,
    // and idempotentHint=true; names never imply trust.
    assert_eq!(
        discovered_tool_idempotency("ordinary-trusted", "2025-03-26", true, Some(true)).await,
        IdempotencyClass::Idempotent
    );
    assert_eq!(
        discovered_tool_idempotency("newer-trusted", "2028-02-29", true, Some(true)).await,
        IdempotencyClass::Idempotent
    );
    assert_eq!(
        discovered_tool_idempotency("fixture-untrusted", "2025-03-26", false, Some(true)).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("legacy-trusted", "2024-11-05", true, Some(true)).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("impossible-date", "2025-04-31", true, Some(true)).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("absent-hint", "2025-03-26", true, None).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("false-hint", "2025-03-26", true, Some(false)).await,
        IdempotencyClass::NonIdempotent
    );
}
