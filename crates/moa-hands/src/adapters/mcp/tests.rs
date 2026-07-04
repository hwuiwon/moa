use std::collections::HashMap;

use moa_core::{McpServerConfig, McpTransportConfig};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{MCPClient, flatten_call_result};

#[tokio::test]
async fn flatten_tool_result_aggregates_text_items() {
    let output = flatten_call_result(json!({
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "text", "text": "world" }
        ]
    }));
    assert_eq!(output.to_text(), "hello\n\nworld");
    assert!(!output.is_error);
}

#[tokio::test]
async fn http_client_sends_headers_and_parses_jsonrpc() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            if request_index == 2 {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer token")
                );
            }
            let body = if request_index == 0 {
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
            } else if request_index == 1 {
                r"{}"
            } else {
                r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}]}}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let client = MCPClient::connect(&McpServerConfig {
        name: "remote".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        ..McpServerConfig::default()
    })
    .await
    .unwrap();

    let output = client
        .call_tool(
            "ping",
            json!({}),
            HashMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
        )
        .await
        .unwrap();
    assert_eq!(output.to_text(), "pong");
}

#[tokio::test]
async fn http_client_parses_sse_tool_response() {
    // Pins: a `text/event-stream` JSON-RPC response is parsed via eventsource-stream.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for request_index in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            let (content_type, body) = if request_index == 0 {
                (
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#
                        .to_string(),
                )
            } else if request_index == 1 {
                ("application/json", "{}".to_string())
            } else {
                (
                    "text/event-stream",
                    "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}}\n\n"
                        .to_string(),
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let client = MCPClient::connect(&McpServerConfig {
        name: "remote".to_string(),
        transport: McpTransportConfig::Http,
        url: Some(format!("http://{addr}")),
        ..McpServerConfig::default()
    })
    .await
    .unwrap();

    let output = client
        .call_tool("ping", json!({}), HashMap::new())
        .await
        .unwrap();
    assert_eq!(output.to_text(), "pong");
}
