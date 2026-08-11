use moa_config::McpCredentialConfig;
use moa_config::McpServerConfig;
use moa_config::MoaConfig;
use moa_config::SecurityProfile;
use moa_core::{
    traits::Identity,
    traits::IdentityType,
    types::completion::ToolInvocation,
    types::identifiers::{ModelId, TenantId, ToolCallId},
    types::security::SensitivityClass,
    types::session::SessionMeta,
    types::tools::IdempotencyClass,
};
use moa_hands::ToolRouter;
use moa_memory_pii::{MockClassifier, PiiResult};
use moa_security::McpEgressGuard;
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
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

pub(super) fn opt_into_development_local_hands(config: &mut MoaConfig) {
    config.local.docker_enabled = false;
    config.security_profile = SecurityProfile::Local;
}

pub(super) fn mcp_egress_guard() -> std::sync::Arc<McpEgressGuard> {
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
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: "guard-required".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            allowed_data_classes: Vec::new(),
            credentials: None,
            trust_tool_annotations: false,
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
async fn deployment_credentials_authenticate_the_full_mcp_exchange_offline() {
    // Pins: stateless discovery, tool discovery, and invocation all
    // use the deployment credential, and tools/call carries the durable MOA
    // ToolCallId rather than a provider transcript identifier.
    const TOOL_CALL_ID: Uuid = Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c499);
    let token_env = format!("MOA_TEST_MCP_TOKEN_{}", Uuid::now_v7().simple());
    unsafe { std::env::set_var(&token_env, "deployment-secret") };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer deployment-secret"),
                "request {request_index} must be authenticated: {request}"
            );
            let expected_method = ["server/discover", "tools/list", "tools/call"][request_index];
            assert!(
                request.contains(&format!("\"method\":\"{expected_method}\"")),
                "request {request_index} used the wrong MCP method: {request}"
            );
            if request_index == 2 {
                assert!(request.contains(&format!("\"moa/toolInvocationId\":\"{TOOL_CALL_ID}\"")));
                assert!(!request.contains("provider-transcript-id"));
            }
            let body = match request_index {
                0 => {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                1 => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"ping","description":"Ping","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}],"ttlMs":300000,"cacheScope":"private"}}"#
                }
                _ => {
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"pong"}]}}"#
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "secure-api".to_string(),
        url: format!("http://{addr}"),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let secured = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: Some("provider-transcript-id".to_string()),
                name: moa_hands::mcp_tool_reference("secure-api", "ping"),
                input: json!({}),
            },
            tool_call_id: ToolCallId(TOOL_CALL_ID),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .unwrap();
    let output = secured.safe_output;

    assert_eq!(output.to_text(), "pong");
    server.await.expect("fake MCP server should finish");
    unsafe { std::env::remove_var(token_env) };
}

#[tokio::test]
async fn cancellation_stops_an_in_flight_authenticated_mcp_call_offline() {
    // Pins: cancellation interrupts an MCP tools/call that has already reached
    // the authenticated server instead of waiting for the transport timeout.
    let token_env = format!("MOA_TEST_MCP_CANCEL_TOKEN_{}", Uuid::now_v7().simple());
    unsafe { std::env::set_var(&token_env, "cancel-secret") };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (call_seen_tx, call_seen_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut call_seen_tx = Some(call_seen_tx);
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer cancel-secret"),
                "request {request_index} must be authenticated"
            );
            if request_index == 2 {
                call_seen_tx.take().unwrap().send(()).unwrap();
                std::future::pending::<()>().await;
            }
            let body = match request_index {
                0 => {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                _ => {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"wait","description":"Wait","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}],"ttlMs":300000,"cacheScope":"private"}}"#
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
    let mut config = local_config(&dir);
    let mut server = connector("slow-api", &format!("http://{addr}"), false);
    server.credentials = Some(McpCredentialConfig::Bearer {
        token_env: token_env.clone(),
    });
    config.mcp_servers = vec![server];
    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let canceller = cancel.clone();
    tokio::spawn(async move {
        call_seen_rx.await.unwrap();
        canceller.cancel();
    });

    let result = timeout(
        Duration::from_secs(2),
        router.execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: Some("provider-transcript-cancel".to_string()),
                name: moa_hands::mcp_tool_reference("slow-api", "wait"),
                input: json!({}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::from_tokens(Some(&cancel), Some(&cancel)),
        }),
    )
    .await
    .expect("cancellation should beat the MCP transport timeout");

    assert!(matches!(result, Err(moa_core::error::MoaError::Cancelled)));
    unsafe { std::env::remove_var(token_env) };
}

#[tokio::test]
async fn discovered_mcp_schema_rejects_malformed_input_without_server_dispatch() {
    // Pins: discovered MCP input schemas are enforced before both reviewed/recovery dispatch
    // and the server call; a valid invocation still reaches the same production route.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"server/discover\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                1 => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"lookup_filing","description":"Lookup filing","inputSchema":{"type":"object","properties":{"item_key":{"type":"string"}},"required":["item_key"],"additionalProperties":false}}],"ttlMs":300000,"cacheScope":"private"}}"#
                }
                _ => {
                    assert!(request.contains("\"method\":\"tools/call\""));
                    // The server must be asked for the name IT published, never
                    // the server-qualified reference MOA registered the tool
                    // under: qualification is a local naming concern and a
                    // connector would reject the qualified name outright.
                    assert!(
                        request.contains("\"name\":\"lookup_filing\""),
                        "tools/call must carry the remote tool name: {request}"
                    );
                    assert!(request.contains("\"item_key\":\"AAPL-10K\""));
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","content":[{"type":"text","text":"filing"}]}}"#
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "filings".to_string(),
        url: format!("http://{addr}"),
        credentials: None,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let malformed = router
        .execute_authorized_with_recovery(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: Some("reviewed-provider-call-bad".to_string()),
                name: moa_hands::mcp_tool_reference("filings", "lookup_filing"),
                input: json!({"item_key": 7}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
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

    let secured_2 = router
        .execute_authorized_with_recovery(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: Some("reviewed-provider-call-good".to_string()),
                name: moa_hands::mcp_tool_reference("filings", "lookup_filing"),
                input: json!({"item_key": "AAPL-10K"}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect("valid MCP input should dispatch");

    let output = secured_2.safe_output;
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "secure-api".to_string(),
        url: "http://127.0.0.1:1".to_string(),
        credentials: Some(McpCredentialConfig::Bearer {
            token_env: token_env.clone(),
        }),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
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
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"server/discover\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                1 => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"explode","description":"Fails","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}],"ttlMs":300000,"cacheScope":"private"}}"#
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "http-api".to_string(),
        url: format!("http://{addr}"),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
        credentials: None,
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    let error = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: None,
                name: moa_hands::mcp_tool_reference("http-api", "explode"),
                input: json!({}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .unwrap_err();

    // Printing the observed error matters here specifically: the other way
    // this dispatch fails is `unknown tool: ...` from a mis-qualified
    // reference, and a bare contains-check would report both as the same
    // uninformative red.
    assert!(
        error.to_string().contains("boom"),
        "the JSON-RPC error should surface verbatim, got: {error}"
    );
}

#[tokio::test]
async fn connector_tool_named_like_a_local_tool_is_qualified_apart_offline() {
    // Pins: a connector publishing a tool called `bash` no longer takes the whole
    // router down at construction, and no longer shadows the local `bash`. Both
    // are served, under distinct names, because one connector's naming choice
    // must not be able to remove a capability from every other tenant.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"server/discover\""));
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                _ => {
                    assert!(request.contains("\"method\":\"tools/list\""));
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"bash","description":"Remote shell","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}],"ttlMs":300000,"cacheScope":"private"}}"#
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "shadow-api".to_string(),
        url: format!("http://{addr}"),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
        credentials: None,
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("a connector name collision must not fail router construction");

    let qualified = moa_hands::mcp_tool_reference("shadow-api", "bash");
    assert!(
        router.has_tool(&qualified),
        "the connector tool must be registered under its server-qualified reference"
    );
    assert!(
        router.tool_requires_sandbox("bash"),
        "the local hand-routed `bash` must still be the tool registered as `bash`"
    );
    assert!(
        !router.tool_requires_sandbox(&qualified),
        "the connector tool must route to its server, not to a sandbox"
    );
    assert_eq!(
        router
            .tool_definition(&qualified)
            .expect("connector tool definition")
            .description,
        "Remote shell",
        "the qualified reference must resolve to the connector's own tool"
    );
}

#[tokio::test]
async fn router_discovers_and_calls_streamable_http_tools_with_sse_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let expected_method = ["server/discover", "tools/list", "tools/call"][request_index];
            assert!(request.contains(&format!("\"method\":\"{expected_method}\"")));
            let (content_type, body) = match request_index {
                0 => (
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                        .to_string(),
                ),
                1 => (
                    "text/event-stream",
                    concat!(
                        ": keep-alive\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"resultType\":\"complete\",\"tools\":[{\"name\":\"sse_echo\",\"description\":\"Echoes text\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"],\"additionalProperties\":false}}],\"ttlMs\":300000,\"cacheScope\":\"private\"}}\n\n"
                    )
                    .to_string(),
                ),
                _ => (
                    "text/event-stream",
                    concat!(
                        ": keep-alive\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"resultType\":\"complete\",\"content\":[{\"type\":\"text\",\"text\":\"sse-pong\"}]}}\n\n"
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "sse-api".to_string(),
        url: format!("http://{addr}"),
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
        credentials: None,
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .unwrap();
    assert!(
        router
            .tool_schemas()
            .iter()
            .any(|tool| tool.get("name").and_then(|value| value.as_str())
                == Some(moa_hands::mcp_tool_reference("sse-api", "sse_echo").as_str()))
    );

    let secured_3 = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: None,
                name: moa_hands::mcp_tool_reference("sse-api", "sse_echo"),
                input: json!({ "text": "hello" }),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .unwrap();

    let output = secured_3.safe_output;

    assert_eq!(output.to_text(), "sse-pong");
}

async fn discovered_tool_idempotency(
    server_name: &str,
    trust_tool_annotations: bool,
    idempotent_hint: Option<bool>,
) -> IdempotencyClass {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("read fake MCP address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept MCP request");
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.expect("read MCP request");
            let request = String::from_utf8_lossy(&buffer[..bytes]);
            let body = match request_index {
                0 => {
                    assert!(request.contains("\"method\":\"server/discover\""));
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "resultType": "complete",
                            "supportedVersions": ["2026-07-28"],
                            "capabilities": {"tools": {}},
                            "ttlMs": 60_000,
                            "cacheScope": "private"
                        }
                    })
                    .to_string()
                }
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
                        "result": {
                            "resultType": "complete",
                            "tools": [tool],
                            "ttlMs": 300_000,
                            "cacheScope": "private"
                        }
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
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: server_name.to_string(),
        url: format!("http://{addr}"),
        credentials: None,
        trust_tool_annotations,
        allowed_data_classes: Vec::new(),
    }];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("build router from discovered MCP tool");
    server.await.expect("fake MCP server should finish");
    router
        .tool_definition(&moa_hands::mcp_tool_reference(
            server_name,
            "retry_safe_read",
        ))
        .expect("discovered tool should be registered")
        .idempotency_class
}

#[tokio::test]
async fn discovery_trusts_idempotent_hint_only_for_explicit_capable_server() {
    // Pins: with one hard-cut protocol revision, retry safety requires explicit
    // per-server trust and idempotentHint=true; names never imply trust.
    assert_eq!(
        discovered_tool_idempotency("ordinary-trusted", true, Some(true)).await,
        IdempotencyClass::Idempotent
    );
    assert_eq!(
        discovered_tool_idempotency("fixture-untrusted", false, Some(true)).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("absent-hint", true, None).await,
        IdempotencyClass::NonIdempotent
    );
    assert_eq!(
        discovered_tool_idempotency("false-hint", true, Some(false)).await,
        IdempotencyClass::NonIdempotent
    );
}

// ---------------------------------------------------------------------------
// Connector health, determinism, and lazy discovery
// ---------------------------------------------------------------------------

/// A fake MCP server that answers by JSON-RPC method and can be taken down.
///
/// Routing on the method rather than on a request counter is what lets one
/// server serve a discovery pass, a refresh, and a tool call in any order — the
/// orders these tests exist to vary.
struct MethodRoutedMcpServer {
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    tools_json: std::sync::Arc<tokio::sync::RwLock<String>>,
}

impl MethodRoutedMcpServer {
    /// Changes what subsequent discovery calls observe.
    async fn publish_tools(&self, tools_json: &str) {
        *self.tools_json.write().await = tools_json.to_string();
    }

    /// Stops the server and waits for the port to stop accepting.
    async fn shut_down(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // Give the accept loop a moment to drop the listener so a later connect
        // fails rather than racing a half-closed socket.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Serves `tools/list` with exactly `tools_json` until shut down.
async fn spawn_method_routed_mcp_server(tools_json: &str) -> MethodRoutedMcpServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let tools_json = std::sync::Arc::new(tokio::sync::RwLock::new(tools_json.to_string()));
    let served_tools = std::sync::Arc::clone(&tools_json);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = &mut shutdown_rx => return,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut socket, _)) = accepted else {
                return;
            };
            let mut buffer = vec![0_u8; 8192];
            let Ok(bytes) = socket.read(&mut buffer).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let request_body = request
                .split_once("\r\n\r\n")
                .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok());
            let Some(request_body) = request_body else {
                continue;
            };
            let id = request_body["id"].clone();
            let method = request_body["method"].as_str().unwrap_or_default();
            let body = if method == "server/discover" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resultType": "complete",
                        "supportedVersions": ["2026-07-28"],
                        "capabilities": {"tools": {}},
                        "ttlMs": 60_000,
                        "cacheScope": "private"
                    }
                })
                .to_string()
            } else if method == "tools/list" {
                let tools_json = served_tools.read().await;
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resultType": "complete",
                        "tools": serde_json::from_str::<serde_json::Value>(&tools_json)
                            .unwrap_or_else(|_| json!([])),
                        "ttlMs": 300_000,
                        "cacheScope": "private"
                    }
                })
                .to_string()
            } else if method == "tools/call" {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "ok"}]
                    }
                })
                .to_string()
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "Method not found"}
                })
                .to_string()
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    MethodRoutedMcpServer {
        url: format!("http://{addr}"),
        shutdown: Some(shutdown_tx),
        tools_json,
    }
}

fn connector(name: &str, url: &str, required: bool) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        url: url.to_string(),
        credentials: None,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
        required,
        discovery: moa_config::McpDiscoveryMode::Eager,
    }
}

fn local_config(dir: &tempfile::TempDir) -> MoaConfig {
    let mut config = MoaConfig::default();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    opt_into_development_local_hands(&mut config);
    config
}

/// A URL on the loopback discard port, which never accepts a connection.
fn unreachable_url() -> String {
    "http://127.0.0.1:1".to_string()
}

#[tokio::test]
async fn an_optional_connector_outage_leaves_every_other_connector_serving_offline() {
    // Pins: one unreachable optional connector removes only its own tools. The
    // router still builds, the healthy connector's tools are still offered, and
    // the outage is reported as typed health rather than as a startup failure —
    // the whole point of marking a connector optional.
    let healthy = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    config.mcp_servers = vec![
        connector("reachable", &healthy.url, false),
        connector("down", &unreachable_url(), false),
    ];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("an optional connector outage must not fail router construction");

    assert!(
        router.has_tool(&moa_hands::mcp_tool_reference("reachable", "search")),
        "the healthy connector's tool must still be offered"
    );

    let health = router.mcp_connector_health().await;
    assert!(
        matches!(
            health.get("reachable"),
            Some(moa_hands::McpConnectorHealth::Ready { tools: 1, .. })
        ),
        "the healthy connector must report Ready with its tool count, got: {:?}",
        health.get("reachable")
    );
    assert!(
        matches!(
            health.get("down"),
            Some(moa_hands::McpConnectorHealth::Unavailable { .. })
        ),
        "the failed optional connector must report Unavailable, got: {:?}",
        health.get("down")
    );
    assert!(
        !router
            .tool_names()
            .iter()
            .any(|name| name.starts_with(&moa_hands::mcp_tool_reference("down", ""))),
        "an unavailable connector must contribute no tools"
    );
}

#[tokio::test]
async fn a_required_connector_outage_fails_startup_with_its_typed_health_offline() {
    // Pins: the same outage that is survivable for an optional connector is a
    // startup failure for a required one, and the failure names the connector
    // and the typed health state it reached. A deployment that silently dropped
    // a required integration would be indistinguishable from one that never
    // configured it.
    let healthy = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    config.mcp_servers = vec![
        connector("reachable", &healthy.url, false),
        connector("must-work", &unreachable_url(), true),
    ];

    let error = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None).await;
    let error = match error {
        Ok(_) => panic!("a required connector outage must fail startup"),
        Err(error) => error,
    };

    match error {
        moa_core::error::MoaError::ConfigError(message) => {
            assert!(
                message.contains("must-work"),
                "the failure must name the required connector: {message}"
            );
            assert!(
                message.contains("unavailable"),
                "the failure must carry the connector's typed health state: {message}"
            );
        }
        other => panic!("expected a ConfigError carrying connector health, got {other:?}"),
    }
}

#[tokio::test]
async fn a_connector_catalog_is_identical_whatever_order_the_server_lists_tools_in_offline() {
    // Pins: "same inputs and revision yield the same schemas and order" is a
    // property of the catalog, not of the remote server. Two servers publishing
    // the same tools in opposite `tools/list` order must produce one identical
    // catalog revision and one identical offered order.
    let forward = spawn_method_routed_mcp_server(
        r#"[{"name":"alpha","description":"A","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},{"name":"zulu","description":"Z","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;
    let reversed = spawn_method_routed_mcp_server(
        r#"[{"name":"zulu","description":"Z","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},{"name":"alpha","description":"A","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let forward_dir = tempdir().unwrap();
    let mut forward_config = local_config(&forward_dir);
    forward_config.mcp_servers = vec![connector("catalog", &forward.url, false)];
    let forward_router = ToolRouter::from_config(&forward_config, Some(mcp_egress_guard()), None)
        .await
        .expect("forward-order router");

    let reversed_dir = tempdir().unwrap();
    let mut reversed_config = local_config(&reversed_dir);
    reversed_config.mcp_servers = vec![connector("catalog", &reversed.url, false)];
    let reversed_router = ToolRouter::from_config(&reversed_config, Some(mcp_egress_guard()), None)
        .await
        .expect("reversed-order router");

    assert_eq!(
        forward_router.mcp_catalog_revision(),
        reversed_router.mcp_catalog_revision(),
        "the catalog revision must not depend on the server's tools/list order"
    );

    let offered = |router: &ToolRouter| {
        router
            .tool_schemas()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| name.starts_with(moa_hands::MCP_TOOL_REFERENCE_PREFIX))
                    .map(ToString::to_string)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        offered(&forward_router),
        vec![
            moa_hands::mcp_tool_reference("catalog", "alpha"),
            moa_hands::mcp_tool_reference("catalog", "zulu"),
        ]
    );
    assert_eq!(offered(&forward_router), offered(&reversed_router));
}

#[tokio::test]
async fn a_refresh_that_fails_keeps_serving_the_last_known_good_tools_offline() {
    // Pins: a connector that was healthy and then goes down is Degraded, not
    // Unavailable, and its previously discovered tools keep being offered.
    // Dropping them on one failed refresh would let an unrelated transient error
    // silently shrink the model's loadout mid-session.
    let mut server = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    config.mcp_servers = vec![connector("flaky", &server.url, false)];
    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a healthy connector");
    let qualified = moa_hands::mcp_tool_reference("flaky", "search");
    assert!(
        router.has_tool(&qualified),
        "the connector's tool must be discovered before its outage can be tested; \
         registered tools: {:?}",
        router.tool_names()
    );
    let healthy_revision = router.mcp_catalog_revision();

    server.shut_down().await;
    let refresh = router.refresh_mcp_catalog().await;

    assert!(
        matches!(
            refresh.health.get("flaky"),
            Some(moa_hands::McpConnectorHealth::Degraded { tools: 1, .. })
        ),
        "a connector with a previous success must degrade, not vanish, got: {:?}",
        refresh.health.get("flaky")
    );
    assert!(
        router.has_tool(&qualified),
        "the last-known-good tool must still be offered while the connector is down"
    );
    assert_eq!(
        router.mcp_catalog_revision(),
        healthy_revision,
        "retaining last-known-good tools must not change the catalog revision"
    );
}

#[tokio::test]
async fn an_empty_refresh_keeps_serving_the_last_known_good_catalog_offline() {
    // Pins: an empty tools/list response is not an intentional withdrawal
    // protocol. Treating it as one lets a transient connector bug erase a
    // working catalog, so the empty candidate is quarantined and the exact
    // prior snapshot remains active.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;
    let dir = tempdir().expect("catalog tempdir");
    let mut config = local_config(&dir);
    config.mcp_servers = vec![connector("flaky", &server.url, false)];
    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a healthy connector");
    let qualified = moa_hands::mcp_tool_reference("flaky", "search");
    let active = router.activated_catalog();
    let pin = active.pin().expect("active pin");

    server.publish_tools("[]").await;
    let refresh = router.refresh_mcp_catalog().await;

    assert!(
        matches!(
            refresh.health.get("flaky"),
            Some(moa_hands::McpConnectorHealth::Quarantined { tools: 1, defects, .. })
                if matches!(
                    defects.as_slice(),
                    [moa_hands::CatalogDefect::NoOfferableTools { rejected: 0, .. }]
                )
        ),
        "empty discovery must quarantine with one retained tool: {:?}",
        refresh.health.get("flaky")
    );
    assert!(router.has_tool(&qualified));
    assert_eq!(refresh.activation.pin, pin);
    assert_eq!(
        active.pin().expect("retained snapshot pin"),
        router
            .activated_catalog()
            .pin()
            .expect("published snapshot pin")
    );
}

#[tokio::test]
async fn a_lazy_connector_contributes_no_tools_until_the_first_refresh_offline() {
    // Pins: lazy discovery genuinely defers. A lazily configured connector is
    // Pending after construction — not Unavailable, because nothing was tried —
    // and its tools appear only once a refresh discovers them.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    let mut lazy = connector("later", &server.url, false);
    lazy.discovery = moa_config::McpDiscoveryMode::Lazy;
    config.mcp_servers = vec![lazy];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a lazily discovered connector");
    let qualified = moa_hands::mcp_tool_reference("later", "search");

    assert!(
        matches!(
            router.mcp_connector_health().await.get("later"),
            Some(moa_hands::McpConnectorHealth::Pending)
        ),
        "a lazy connector must be Pending, not Unavailable, before anything is attempted"
    );
    assert!(
        !router.has_tool(&qualified),
        "a lazy connector must contribute no tools at startup"
    );

    let refresh = router.refresh_mcp_catalog().await;

    assert!(
        matches!(
            refresh.health.get("later"),
            Some(moa_hands::McpConnectorHealth::Ready { tools: 1, .. })
        ),
        "the first refresh must discover the lazy connector, got: {:?}",
        refresh.health.get("later")
    );
    assert!(
        router.has_tool(&qualified),
        "a discovered lazy connector's tools must become available without a restart"
    );
}

#[tokio::test]
async fn a_required_connector_cannot_be_configured_for_lazy_discovery_offline() {
    // Pins: `required` means "verified at startup". Allowing a required
    // connector to be discovered lazily would let startup succeed without ever
    // having contacted the integration the operator declared mandatory.
    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    let mut server = connector("must-work", &unreachable_url(), true);
    server.discovery = moa_config::McpDiscoveryMode::Lazy;
    config.mcp_servers = vec![server];

    let error = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None).await;
    let error = match error {
        Ok(_) => panic!("required plus lazy must be rejected"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            moa_core::error::MoaError::ConfigError(ref message)
                if message.contains("must-work") && message.contains("lazily")
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn two_connectors_configured_under_one_name_are_rejected_offline() {
    // Pins: duplicate server names are a startup failure rather than a silent
    // overwrite. Without this, the second entry would replace the first's
    // configuration — including its credential scope — while both appeared
    // configured.
    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    config.mcp_servers = vec![
        connector("same", &unreachable_url(), false),
        connector("same", &unreachable_url(), false),
    ];

    let error = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None).await;
    let error = match error {
        Ok(_) => panic!("duplicate connector names must be rejected"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error,
            moa_core::error::MoaError::ConfigError(ref message)
                if message.contains("duplicate MCP server name") && message.contains("same")
        ),
        "unexpected error: {error:?}"
    );
}

#[tokio::test]
async fn a_connector_schema_change_changes_the_tool_capability_revision_offline() {
    // Pins: the schema hash a connector tool registers under tracks the schema
    // itself, so a server that changes a tool's input schema produces a
    // different revision under an unchanged tool name. That revision is the
    // capability version a pinned execution run matches against, which is what
    // makes a mid-flight schema change fail closed instead of silently
    // invoking a different contract.
    let original = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{"q":{"type":"string"}},"additionalProperties":false}}]"#,
    )
    .await;
    let changed = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{"query":{"type":"string"}},"additionalProperties":false}}]"#,
    )
    .await;

    let original_dir = tempdir().unwrap();
    let mut original_config = local_config(&original_dir);
    original_config.mcp_servers = vec![connector("api", &original.url, false)];
    let original_router = ToolRouter::from_config(&original_config, Some(mcp_egress_guard()), None)
        .await
        .expect("router before the schema change");

    let changed_dir = tempdir().unwrap();
    let mut changed_config = local_config(&changed_dir);
    changed_config.mcp_servers = vec![connector("api", &changed.url, false)];
    let changed_router = ToolRouter::from_config(&changed_config, Some(mcp_egress_guard()), None)
        .await
        .expect("router after the schema change");

    let qualified = moa_hands::mcp_tool_reference("api", "search");
    // Asserted separately rather than with `&&`: a compound precondition that
    // fails tells you nothing about which router was missing the tool.
    assert!(
        original_router.has_tool(&qualified),
        "the pre-change router must serve the tool, got: {:?}",
        original_router.tool_names()
    );
    assert!(
        changed_router.has_tool(&qualified),
        "the post-change router must serve the same reference, got: {:?}",
        changed_router.tool_names()
    );
    assert_ne!(
        original_router.mcp_catalog_revision(),
        changed_router.mcp_catalog_revision(),
        "a changed input schema must change the catalog revision under the same tool name"
    );
}

#[tokio::test]
async fn a_refresh_republishes_the_prompt_schemas_a_turn_compiles_from_offline() {
    // Pins: a catalog refresh reaches the prompt. The schemas a turn compiles
    // from are read back from the router rather than captured once, so a
    // connector discovered after startup becomes offerable without a restart —
    // and a connector's tools cannot linger in a prompt after the catalog
    // withdrew them.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    let mut lazy = connector("later", &server.url, false);
    lazy.discovery = moa_config::McpDiscoveryMode::Lazy;
    config.mcp_servers = vec![lazy];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a lazily discovered connector");
    let qualified = moa_hands::mcp_tool_reference("later", "search");
    let offered = |router: &ToolRouter| {
        router
            .tool_schema_snapshot()
            .iter()
            .filter_map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .collect::<Vec<_>>()
    };
    assert!(
        !offered(&router).contains(&qualified),
        "an undiscovered connector must not be in the prompt schemas"
    );

    router.refresh_mcp_catalog().await;

    assert!(
        offered(&router).contains(&qualified),
        "the refreshed catalog must be visible in the prompt schemas"
    );
}

#[tokio::test]
async fn a_lazily_discovered_connector_clears_its_permission_pattern_warning_offline() {
    // Pins: the report tracks the live catalog rather than a startup snapshot.
    // A pattern written for a lazy connector's tools genuinely governs nothing
    // until those tools exist, and must stop being reported once they do —
    // otherwise the warning becomes permanent noise that operators learn to
    // ignore, which is worse than not having it.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"external_action","description":"External","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    let mut lazy = connector("crm", &server.url, false);
    lazy.discovery = moa_config::McpDiscoveryMode::Lazy;
    config.mcp_servers = vec![lazy];
    config.permissions.admin_review = vec![format!(
        "{}*",
        moa_hands::mcp_tool_reference("crm", "external_")
    )];

    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a lazy connector");
    assert_eq!(
        router.unmatched_permission_patterns().len(),
        1,
        "before discovery the pattern genuinely governs nothing"
    );

    router.refresh_mcp_catalog().await;

    assert!(
        router.unmatched_permission_patterns().is_empty(),
        "once the connector's tools exist the pattern governs them: {:?}",
        router.unmatched_permission_patterns()
    );
}

#[tokio::test]
async fn a_permission_pattern_is_checked_when_no_connector_is_configured_offline() {
    // Pins the construction-time check specifically. A deployment with no MCP
    // servers never runs connector discovery, so the recompute that happens
    // after a discovery pass never fires — and the pattern check has to happen
    // during construction or it never happens at all for the majority of
    // deployments. This is the arm the connector-bearing tests cannot reach.
    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    assert!(
        config.mcp_servers.is_empty(),
        "this test's whole point is the no-connector path"
    );
    config.permissions.always_deny = vec!["bash".to_string()];
    config.permissions.admin_review = vec!["definitely_not_a_registered_tool".to_string()];

    let router = ToolRouter::from_config(&config, None, None)
        .await
        .expect("router without connectors");

    let unmatched = router.unmatched_permission_patterns();
    assert_eq!(
        unmatched
            .iter()
            .map(|entry| (entry.field, entry.pattern.as_str()))
            .collect::<Vec<_>>(),
        vec![(
            "permissions.admin_review",
            "definitely_not_a_registered_tool"
        )],
        "the pattern governing a local tool must not be reported, the other must: {unmatched:?}"
    );
}

#[tokio::test]
async fn dispatching_a_connectors_published_name_says_which_reference_to_use_offline() {
    // Pins the diagnostic, not just the refusal. A caller that resolved a tool
    // through a connector's own vocabulary rather than the registry's arrives
    // with a name that looks right and is not, and a bare "unknown tool" sends
    // the reader hunting for a typo that does not exist. This is the exact
    // failure a live execution run produced, so the message has to name the
    // reference that would have worked.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"screen_company","description":"Screen","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]"#,
    )
    .await;

    let dir = tempdir().unwrap();
    let mut config = local_config(&dir);
    config.mcp_servers = vec![connector("screener", &server.url, false)];
    let router = ToolRouter::from_config(&config, Some(mcp_egress_guard()), None)
        .await
        .expect("router with a connector");

    let error = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: None,
                // The name the SERVER publishes — not the registered reference.
                name: "screen_company".to_string(),
                input: json!({}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("a connector's published name must not resolve");

    let message = error.to_string();
    assert!(
        message.contains("screen_company"),
        "the failure must name what was dispatched, got: {message}"
    );
    assert!(
        message.contains(&moa_hands::mcp_tool_reference("screener", "screen_company")),
        "the failure must name the reference that would have worked, got: {message}"
    );

    // An ordinary typo must NOT be dressed up as a qualification problem.
    let plain = router
        .execute_authorized(moa_hands::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            workspace_scope: None,
            invocation: &ToolInvocation {
                id: None,
                name: "no_such_tool_anywhere".to_string(),
                input: json!({}),
            },
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: moa_hands::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("an unknown name must still fail")
        .to_string();
    assert!(
        !plain.contains("server-qualified"),
        "a name no connector publishes must not be explained as a qualification mistake: {plain}"
    );
}

// ---------------------------------------------------------------------------
// Staged candidate catalogs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn replicas_converge_on_one_activated_snapshot_and_keep_it_through_a_failed_refresh_offline()
{
    // Pins: the pin is a property of the catalog, not of the process that
    // derived it, and a refresh failure cannot move it. Two independently built
    // routers over the same connectors must produce byte-identical activated
    // snapshots so prompt, policy, and dispatch checks agree across replicas. A
    // connector outage must leave the snapshot exactly where it was rather than
    // shrinking it to whatever survived.
    let server = spawn_method_routed_mcp_server(
        r#"[{"name":"search","description":"Search","inputSchema":{"type":"object","properties":{"q":{"type":"string"}},"additionalProperties":false}}]"#,
    )
    .await;

    let first_dir = tempdir().expect("first replica tempdir");
    let mut first_config = local_config(&first_dir);
    first_config.mcp_servers = vec![connector("shared", &server.url, false)];
    let first = ToolRouter::from_config(&first_config, Some(mcp_egress_guard()), None)
        .await
        .expect("first replica");

    let second_dir = tempdir().expect("second replica tempdir");
    let mut second_config = local_config(&second_dir);
    second_config.mcp_servers = vec![connector("shared", &server.url, false)];
    let second = ToolRouter::from_config(&second_config, Some(mcp_egress_guard()), None)
        .await
        .expect("second replica");

    let first_pin = first.activated_catalog().pin().expect("first pin");
    let second_pin = second.activated_catalog().pin().expect("second pin");
    assert_eq!(
        first_pin, second_pin,
        "two replicas over the same catalog must activate one identical snapshot"
    );
    let mut server = server;
    server.shut_down().await;
    let refresh = second.refresh_mcp_catalog().await;

    assert_eq!(
        refresh
            .health
            .get("shared")
            .expect("shared connector health")
            .state(),
        "degraded",
        "an unreachable connector with last-known-good tools degrades"
    );
    assert_eq!(
        refresh.activation.pin, first_pin,
        "a failed refresh must preserve the activated snapshot exactly"
    );
    assert_eq!(
        second.activated_catalog().pin().expect("pin after outage"),
        first_pin,
    );
}
