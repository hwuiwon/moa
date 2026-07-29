use std::collections::HashMap;

use moa_config::McpServerConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MCPClient, flatten_call_result, protocol_supports_tool_annotations};

fn request_body(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request should contain a body delimiter");
    serde_json::from_str(body).expect("HTTP request body should be JSON")
}

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

#[test]
fn tool_annotations_require_canonical_capable_protocol_revision() {
    // Pins: version gating accepts the annotation revision and newer date revisions,
    // while legacy, malformed, and impossible calendar dates cannot become retry-safe.
    assert!(protocol_supports_tool_annotations("2025-03-26"));
    assert!(protocol_supports_tool_annotations("2025-06-18"));
    assert!(protocol_supports_tool_annotations("2028-02-29"));
    assert!(!protocol_supports_tool_annotations("2024-11-05"));
    assert!(!protocol_supports_tool_annotations("latest"));
    assert!(!protocol_supports_tool_annotations("2025-3-26"));
    assert!(!protocol_supports_tool_annotations("+2025-03-26"));
    assert!(!protocol_supports_tool_annotations("2025-13-01"));
    assert!(!protocol_supports_tool_annotations("2025-04-31"));
    assert!(!protocol_supports_tool_annotations("2025-02-29"));
    assert!(!protocol_supports_tool_annotations("2028-02-30"));
}

#[tokio::test]
async fn http_client_sends_headers_and_parses_jsonrpc() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let bytes = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
            let request_json = request_body(&request);
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer token"),
                "authentication must be present on request {request_index}"
            );
            if request_index == 0 {
                assert_eq!(
                    request_json["params"]["protocolVersion"],
                    json!("2025-03-26")
                );
            } else if request_index == 3 {
                assert_eq!(
                    request_json["params"],
                    json!({
                        "name": "ping",
                        "arguments": {},
                        "_meta": {"moa/toolInvocationId": "provider-call-17"}
                    })
                );
            }
            let body = if request_index == 0 {
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#
            } else if request_index == 1 {
                r"{}"
            } else if request_index == 2 {
                r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#
            } else {
                r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let client = MCPClient::connect(
        &McpServerConfig {
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: "remote".to_string(),
            url: format!("http://{addr}"),
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: Vec::new(),
        },
        HashMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
    )
    .await
    .unwrap();
    assert_eq!(client.negotiated_protocol_version(), "2025-06-18");
    assert!(client.list_tools().await.unwrap().is_empty());

    let output = client
        .call_tool("ping", json!({}), Some("provider-call-17"), None)
        .await
        .unwrap();
    assert_eq!(output.to_text(), "pong");
    server.await.expect("fake MCP server should finish");
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

    let client = MCPClient::connect(
        &McpServerConfig {
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: "remote".to_string(),
            url: format!("http://{addr}"),
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: Vec::new(),
        },
        HashMap::new(),
    )
    .await
    .unwrap();

    let output = client
        .call_tool("ping", json!({}), None, None)
        .await
        .unwrap();
    assert_eq!(output.to_text(), "pong");
}

#[tokio::test]
async fn local_cancellation_sends_an_authenticated_protocol_notification() {
    // Pins: cancelling one allocated tools/call request sends the MCP
    // notifications/cancelled message with that exact JSON-RPC ID and the same
    // authentication as every other exchange. The returned error describes the
    // local wait only; it does not claim the remote side effect stopped.
    #[derive(Debug)]
    struct RecordedRequest {
        authenticated: bool,
        body: Value,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (recorded_tx, mut recorded_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let recorded_tx = recorded_tx.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 4096];
                let bytes = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let body = request_body(&request);
                let method = body["method"].as_str().unwrap_or_default().to_string();
                recorded_tx
                    .send(RecordedRequest {
                        authenticated: request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer cancel-token"),
                        body,
                    })
                    .unwrap();

                if method == "tools/call" {
                    std::future::pending::<()>().await;
                }
                let response_body = if method == "initialize" {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}"#
                } else {
                    "{}"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });

    let client = std::sync::Arc::new(
        MCPClient::connect(
            &McpServerConfig {
                required: false,
                discovery: moa_config::McpDiscoveryMode::Eager,
                name: "cancel-server".to_string(),
                url: format!("http://{addr}"),
                credentials: None,
                trust_tool_annotations: false,
                allowed_data_classes: Vec::new(),
            },
            HashMap::from([(
                "Authorization".to_string(),
                "Bearer cancel-token".to_string(),
            )]),
        )
        .await
        .unwrap(),
    );
    let mut requests = vec![
        recorded_rx.recv().await.unwrap(),
        recorded_rx.recv().await.unwrap(),
    ];

    let cancel = CancellationToken::new();
    let call = tokio::spawn({
        let client = std::sync::Arc::clone(&client);
        let cancel = cancel.clone();
        async move {
            client
                .call_tool("write", json!({"value": 1}), None, Some(&cancel))
                .await
        }
    });
    let call_request = tokio::time::timeout(std::time::Duration::from_secs(2), recorded_rx.recv())
        .await
        .expect("tools/call should reach the server")
        .expect("request channel should remain open");
    let request_id = call_request.body["id"].clone();
    assert_eq!(call_request.body["method"], "tools/call");
    requests.push(call_request);
    cancel.cancel();

    let error = tokio::time::timeout(std::time::Duration::from_secs(2), call)
        .await
        .expect("local cancellation should return promptly")
        .expect("call task should not panic")
        .expect_err("cancelled call must return an error");
    assert!(matches!(error, moa_core::error::MoaError::Cancelled));
    assert_eq!(error.to_string(), "operation cancelled by user");

    let cancellation = tokio::time::timeout(std::time::Duration::from_secs(2), recorded_rx.recv())
        .await
        .expect("cancellation notification should reach the server")
        .expect("request channel should remain open");
    assert_eq!(cancellation.body["method"], "notifications/cancelled");
    assert_eq!(cancellation.body["params"]["requestId"], request_id);
    assert!(
        cancellation.body["params"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("stopped waiting"))
    );
    requests.push(cancellation);

    assert!(
        requests.iter().all(|request| request.authenticated),
        "every MCP request, including cancellation, must be authenticated: {requests:?}"
    );
    server.await.unwrap();
}
