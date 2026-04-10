use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    McpCredentialConfig, McpServerConfig, McpTransportConfig, MemoryPath, MemoryScope,
    MemorySearchResult, MemoryStore, MoaConfig, PageSummary, PageType, Result, SessionMeta,
    ToolInvocation, UserId, WikiPage, WorkspaceId,
};
use moa_hands::ToolRouter;
use serde_json::json;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

#[derive(Default)]
struct EmptyMemoryStore;

#[async_trait]
impl MemoryStore for EmptyMemoryStore {
    async fn search(
        &self,
        _query: &str,
        _scope: MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Ok(Vec::new())
    }

    async fn read_page(&self, _path: &MemoryPath) -> Result<WikiPage> {
        Err(moa_core::MoaError::StorageError("not found".to_string()))
    }

    async fn write_page(&self, _path: &MemoryPath, _page: WikiPage) -> Result<()> {
        Ok(())
    }

    async fn delete_page(&self, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    async fn list_pages(
        &self,
        _scope: MemoryScope,
        _filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        Ok(Vec::new())
    }

    async fn get_index(&self, _scope: MemoryScope) -> Result<String> {
        Ok(String::new())
    }

    async fn rebuild_search_index(&self, _scope: MemoryScope) -> Result<()> {
        Ok(())
    }
}

fn session() -> SessionMeta {
    SessionMeta {
        workspace_id: WorkspaceId::new("workspace"),
        user_id: UserId::new("user"),
        model: "claude-sonnet-4-6".to_string(),
        ..SessionMeta::default()
    }
}

#[tokio::test]
async fn router_discovers_stdio_mcp_tools_from_config() {
    let dir = tempdir().unwrap();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
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

    let router = ToolRouter::from_config(&config, memory_store)
        .await
        .unwrap();
    assert!(
        router
            .tool_schemas()
            .iter()
            .any(|tool| tool.get("name").and_then(|value| value.as_str()) == Some("echo"))
    );

    let (_, output) = router
        .execute_authorized(
            &session(),
            &ToolInvocation {
                id: None,
                name: "echo".to_string(),
                input: json!({ "text": "hello" }),
            },
        )
        .await
        .unwrap();

    assert_eq!(output.stdout, "hello");
}

#[tokio::test]
async fn router_injects_mcp_credentials_via_proxy() {
    let token_env = format!("MOA_TEST_MCP_TOKEN_{}", Uuid::new_v4().simple());
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
                1 => r#"{}"#,
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
    let memory_store: Arc<dyn MemoryStore> = Arc::new(EmptyMemoryStore);
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

    let router = ToolRouter::from_config(&config, memory_store)
        .await
        .unwrap();
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

    assert_eq!(output.stdout, "pong");
    unsafe { std::env::remove_var(token_env) };
}
