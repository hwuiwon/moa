use moa_core::{
    McpCredentialConfig, McpServerConfig, McpTransportConfig, MoaConfig, ModelId, SessionMeta,
    TenantId, ToolInvocation,
};
use moa_hands::ToolRouter;
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: TenantId::new(),
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    }
}

#[tokio::test]
async fn from_config_rejects_stdio_mcp_transport_for_deployment() {
    // stdio MCP requires a pod-local child process, which a cloud/Kubernetes
    // deployment must not depend on. `ToolRouter::from_config` (the deployment
    // entry point) therefore fails closed on a stdio MCP server before
    // constructing the router. Local-dev stdio support, if any, lives on
    // `new_local`, which bypasses this validation. HTTP/SSE MCP discovery +
    // execution through `from_config` is covered by the sibling tests below.
    let dir = tempdir().unwrap();
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.mcp_servers = vec![McpServerConfig {
        name: "mock".to_string(),
        transport: McpTransportConfig::Stdio,
        command: Some("python3".to_string()),
        args: vec![
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/mock_mcp_stdio_server.py")
                .display()
                .to_string(),
        ],
        ..McpServerConfig::default()
    }];

    let error = match ToolRouter::from_config(&config).await {
        Ok(_) => panic!("from_config must reject a stdio MCP server in a deployment"),
        Err(error) => error,
    };
    match error {
        moa_core::MoaError::ConfigError(message) => assert!(
            message.contains("stdio transport") && message.contains("local development"),
            "expected the stdio-rejection ConfigError, got: {message}"
        ),
        other => panic!("expected ConfigError rejecting stdio transport, got {other:?}"),
    }
}

#[tokio::test]
async fn router_injects_mcp_credentials_via_proxy() {
    let token_env = format!("MOA_TEST_MCP_TOKEN_{}", Uuid::now_v7().simple());
    unsafe { std::env::set_var(&token_env, "proxy-secret") };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
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
    config.mcp_servers = vec![McpServerConfig {
        name: "secure-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config).await.unwrap();
    let (_, output) = router
        .execute_authorized(
            &session(),
            &ToolInvocation {
                id: None,
                name: "ping".to_string(),
                input: json!({}),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.to_text(), "pong");
    unsafe { std::env::remove_var(token_env) };
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
    config.mcp_servers = vec![McpServerConfig {
        name: "secure-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some("http://127.0.0.1:1".to_string()),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        ..McpServerConfig::default()
    }];

    let error = match ToolRouter::from_config(&config).await {
        Ok(_) => panic!("expected from_config to fail closed on the unset MCP token env var"),
        Err(error) => error,
    };
    match error {
        moa_core::MoaError::MissingEnvironmentVariable(message) => {
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
    config.mcp_servers = vec![McpServerConfig {
        name: "http-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config).await.unwrap();
    let error = router
        .execute_authorized(
            &session(),
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
    config.mcp_servers = vec![McpServerConfig {
        name: "sse-api".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        ..McpServerConfig::default()
    }];

    let router = ToolRouter::from_config(&config).await.unwrap();
    assert!(
        router
            .tool_schemas()
            .iter()
            .any(|tool| tool.get("name").and_then(|value| value.as_str()) == Some("sse_echo"))
    );

    let (_, output) = router
        .execute_authorized(
            &session(),
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
