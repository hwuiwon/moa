use std::collections::HashMap;

use moa_config::McpServerConfig;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    CatalogFreshness, HeaderValueType, MAX_MCP_TOOL_LIST_PAGES, MCPClient, cache_deadline,
    encoded_header_value, flatten_call_result, projected_value, tool_header_projections,
};

fn request_body(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request should contain a body delimiter");
    serde_json::from_str(body).expect("HTTP request body should be JSON")
}

fn request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        candidate.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut buffer = vec![0_u8; 16 * 1024];
    let bytes = socket
        .read(&mut buffer)
        .await
        .expect("read fake MCP request");
    String::from_utf8(buffer[..bytes].to_vec()).expect("request should be UTF-8")
}

async fn write_json_response(socket: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    socket
        .write_all(response.as_bytes())
        .await
        .expect("write fake MCP response");
}

fn config(addr: std::net::SocketAddr) -> McpServerConfig {
    McpServerConfig {
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: "remote".to_string(),
        url: format!("http://{addr}"),
        credentials: None,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }
}

fn assert_modern_request(request: &str, method: &str) -> Value {
    assert_eq!(
        request_header(request, "MCP-Protocol-Version"),
        Some("2026-07-28")
    );
    assert_eq!(request_header(request, "Mcp-Method"), Some(method));
    let accept = request_header(request, "Accept").expect("Accept header");
    assert!(accept.contains("application/json"));
    assert!(accept.contains("text/event-stream"));
    assert!(request_header(request, "Mcp-Session-Id").is_none());
    assert!(request_header(request, "Last-Event-ID").is_none());

    let body = request_body(request);
    assert_eq!(body["method"], method);
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
        "2026-07-28"
    );
    assert_eq!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
        "moa"
    );
    assert!(
        body["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    );
    body
}

#[tokio::test]
async fn flatten_tool_result_aggregates_text_items() {
    let output = flatten_call_result(json!({
        "resultType": "complete",
        "content": [
            { "type": "text", "text": "hello" },
            { "type": "text", "text": "world" }
        ]
    }));
    assert_eq!(output.to_text(), "hello\n\nworld");
    assert!(!output.is_error);
}

#[tokio::test]
async fn modern_http_client_discovers_paginates_and_projects_tool_headers() {
    // Pins: the outbound client follows the stateless 2026-07-28 lifecycle and
    // carries every required Streamable HTTP/body header through a real socket.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut socket, _) = listener.accept().await.expect("accept MCP request");
            let request = read_request(&mut socket).await;
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer token"),
                "authentication must be present on request {request_index}"
            );
            let body = match request_index {
                0 => {
                    let body = assert_modern_request(&request, "server/discover");
                    assert_eq!(body["id"], 1);
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#
                }
                1 => {
                    let body = assert_modern_request(&request, "tools/list");
                    assert!(body["params"].get("cursor").is_none());
                    r#"{"jsonrpc":"2.0","id":2,"result":{"resultType":"complete","tools":[{"name":"ping 世界","description":"Ping","inputSchema":{"type":"object","properties":{"context":{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}},"count":{"type":"integer","x-mcp-header":"Count"},"dryRun":{"type":"boolean","x-mcp-header":"Dry-Run"}}}}],"nextCursor":"page-2","ttlMs":300000,"cacheScope":"public"}}"#
                }
                2 => {
                    let body = assert_modern_request(&request, "tools/list");
                    assert_eq!(body["params"]["cursor"], "page-2");
                    r#"{"jsonrpc":"2.0","id":3,"result":{"resultType":"complete","tools":[{"name":"invalid","inputSchema":{"type":"object","properties":{"value":{"type":"number","x-mcp-header":"Bad"}}}}],"ttlMs":300000,"cacheScope":"public"}}"#
                }
                _ => {
                    let body = assert_modern_request(&request, "tools/call");
                    assert_eq!(body["id"], 4);
                    assert_eq!(body["params"]["name"], "ping 世界");
                    assert_eq!(
                        body["params"]["_meta"]["moa/toolInvocationId"],
                        "provider-call-17"
                    );
                    assert_eq!(
                        request_header(&request, "Mcp-Name"),
                        Some("=?base64?cGluZyDkuJbnlYw=?=")
                    );
                    assert_eq!(
                        request_header(&request, "Mcp-Param-Region"),
                        Some("=?base64?5p2x5Lqs?=")
                    );
                    assert_eq!(request_header(&request, "Mcp-Param-Count"), Some("42"));
                    assert_eq!(request_header(&request, "Mcp-Param-Dry-Run"), Some("true"));
                    r#"{"jsonrpc":"2.0","id":4,"result":{"resultType":"complete","content":[{"type":"text","text":"pong"}],"isError":false}}"#
                }
            };
            write_json_response(&mut socket, body).await;
        }
    });

    let client = MCPClient::connect(
        &config(addr),
        HashMap::from([("Authorization".to_string(), "Bearer token".to_string())]),
    )
    .await
    .expect("connect to modern MCP server");
    assert_eq!(client.protocol_version(), "2026-07-28");
    let tools = client.list_tools().await.expect("list every tool page");
    assert_eq!(tools.len(), 1, "invalid annotated tools must be excluded");
    assert_eq!(tools[0].tool().name, "ping 世界");

    let output = client
        .call_tool(
            "ping 世界",
            json!({"context": {"region": "東京"}, "count": 42, "dryRun": true}),
            Some("provider-call-17"),
            None,
        )
        .await
        .expect("call discovered MCP tool");
    assert_eq!(output.to_text(), "pong");
    server.await.expect("fake MCP server should finish");
}

#[tokio::test]
async fn sse_client_skips_request_notifications_before_matching_final_response() {
    // Pins: request-scoped progress notifications are not mistaken for the
    // final response, and SSE comments are ignored without resumability state.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut socket, _) = listener.accept().await.expect("accept MCP request");
            let request = read_request(&mut socket).await;
            let (content_type, body) = if request_index == 0 {
                assert_modern_request(&request, "server/discover");
                (
                    "application/json",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#.to_string(),
                )
            } else {
                assert_modern_request(&request, "tools/call");
                (
                    "text/event-stream",
                    concat!(
                        ": keep-alive\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progressToken\":1,\"progress\":1,\"total\":2}}\n\n",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"resultType\":\"complete\",\"content\":[{\"type\":\"text\",\"text\":\"pong\"}]}}\n\n"
                    )
                    .to_string(),
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write fake MCP response");
        }
    });

    let client = MCPClient::connect(&config(addr), HashMap::new())
        .await
        .expect("connect to modern MCP server");
    let output = client
        .call_tool("ping", json!({}), None, None)
        .await
        .expect("parse final SSE response");
    assert_eq!(output.to_text(), "pong");
    server.await.expect("fake MCP server should finish");
}

#[tokio::test]
async fn legacy_server_is_rejected_without_initialize_fallback() {
    // Pins: a server that lacks the exact modern revision fails after one
    // server/discover request; no initialize or initialized messages are sent.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept MCP request");
        let request = read_request(&mut socket).await;
        assert_modern_request(&request, "server/discover");
        assert!(!request.contains("initialize"));
        write_json_response(
            &mut socket,
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2025-11-25"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#,
        )
        .await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "modern-only client must not fall back to initialize"
        );
    });

    let error = match MCPClient::connect(&config(addr), HashMap::new()).await {
        Ok(_) => panic!("legacy-only server must not connect"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("does not support required protocol version 2026-07-28")
    );
    server.await.expect("fake MCP server should finish");
}

#[tokio::test]
async fn tool_pagination_stops_at_the_defensive_page_limit() {
    // Pins: a server cannot keep startup or refresh alive with an unbounded
    // sequence of unique nextCursor values.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let server = tokio::spawn(async move {
        for request_index in 0..=MAX_MCP_TOOL_LIST_PAGES {
            let (mut socket, _) = listener.accept().await.expect("accept MCP request");
            let request = read_request(&mut socket).await;
            let body = if request_index == 0 {
                assert_modern_request(&request, "server/discover");
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
            } else {
                let request_body = assert_modern_request(&request, "tools/list");
                json!({
                    "jsonrpc": "2.0",
                    "id": request_body["id"],
                    "result": {
                        "resultType": "complete",
                        "tools": [],
                        "nextCursor": format!("page-{request_index}"),
                        "ttlMs": 300_000,
                        "cacheScope": "private"
                    }
                })
            };
            write_json_response(&mut socket, &body.to_string()).await;
        }
    });

    let client = MCPClient::connect(&config(addr), HashMap::new())
        .await
        .expect("connect to modern MCP server");
    let error = client
        .list_tools()
        .await
        .expect_err("unique cursors must still have a finite page limit");
    assert!(
        error
            .to_string()
            .contains("tools/list exceeded the 100-page safety limit")
    );
    server.await.expect("fake MCP server should finish");
}

#[tokio::test]
async fn http_cancellation_closes_the_request_without_cancel_notification() {
    // Pins: 2026 Streamable HTTP cancellation drops the in-flight response
    // stream and never emits the stdio-only notifications/cancelled message.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake MCP server");
    let addr = listener.local_addr().expect("fake MCP server address");
    let (call_seen_tx, call_seen_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut discover_socket, _) = listener.accept().await.expect("accept discover");
        let discover = read_request(&mut discover_socket).await;
        assert_modern_request(&discover, "server/discover");
        write_json_response(
            &mut discover_socket,
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"}}"#,
        )
        .await;

        let (mut call_socket, _) = listener.accept().await.expect("accept tools/call");
        let call = read_request(&mut call_socket).await;
        assert_modern_request(&call, "tools/call");
        call_seen_tx.send(()).expect("signal tools/call arrival");

        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            call_socket.read(&mut byte),
        )
        .await
        .expect("cancelled request should close its connection")
        .expect("read cancellation EOF");
        assert_eq!(read, 0, "cancellation must close the HTTP response stream");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "HTTP cancellation must not POST notifications/cancelled"
        );
    });

    let client = std::sync::Arc::new(
        MCPClient::connect(&config(addr), HashMap::new())
            .await
            .expect("connect to modern MCP server"),
    );
    let cancellation = CancellationToken::new();
    let call = tokio::spawn({
        let client = std::sync::Arc::clone(&client);
        let cancellation = cancellation.clone();
        async move {
            client
                .call_tool("write", json!({"value": 1}), None, Some(&cancellation))
                .await
        }
    });
    call_seen_rx.await.expect("tools/call should reach server");
    cancellation.cancel();
    let error = call
        .await
        .expect("call task should not panic")
        .expect_err("cancelled call should fail");
    assert!(matches!(error, moa_core::error::MoaError::Cancelled));
    server.await.expect("fake MCP server should finish");
}

#[test]
fn header_projection_validation_enforces_static_primitive_paths() {
    // Pins: only properties reachable through properties-only chains are
    // promoted, with case-insensitive uniqueness and primitive types.
    let valid = tool_header_projections(&json!({
        "type": "object",
        "properties": {
            "outer": {"type": "object", "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"}
            }},
            "count": {"type": "integer", "x-mcp-header": "Count"}
        }
    }))
    .expect("nested properties-only paths are valid");
    assert_eq!(valid.len(), 2);
    assert_eq!(valid[0].path, ["outer", "region"]);

    for invalid in [
        json!(null),
        json!({"properties": {}}),
        json!({"type": "array", "items": {"type": "string"}}),
        json!({"type": "object", "properties": {"n": {"type": "number", "x-mcp-header": "N"}}}),
        json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "Region"}, "b": {"type": "string", "x-mcp-header": "region"}}}),
        json!({"type": "object", "allOf": [{"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
        json!({"type": "object", "properties": {"a": {"type": "string", "x-mcp-header": "bad:name"}}}),
    ] {
        assert!(tool_header_projections(&invalid).is_err());
    }
}

#[test]
fn projected_header_values_follow_safe_integer_and_base64_rules() {
    assert_eq!(
        projected_value(&json!(9_007_199_254_740_991_i64), HeaderValueType::Integer)
            .expect("maximum safe integer"),
        "9007199254740991"
    );
    assert!(projected_value(&json!(9_007_199_254_740_992_u64), HeaderValueType::Integer).is_err());
    assert_eq!(
        encoded_header_value("")
            .expect("empty header is valid")
            .to_str()
            .expect("empty ASCII header"),
        ""
    );
    assert_eq!(
        encoded_header_value(" padded ")
            .expect("encode padded header")
            .to_str()
            .expect("ASCII encoded header"),
        "=?base64?IHBhZGRlZCA=?="
    );
    assert_eq!(
        encoded_header_value("=?base64?literal?=")
            .expect("escape sentinel-shaped header")
            .to_str()
            .expect("ASCII encoded header"),
        "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
    );
}

#[test]
fn catalog_freshness_uses_the_earliest_advertised_expiry_including_zero_offline() {
    // Pins: discovery and every tools/list page constrain the same effective
    // catalog lifetime; ttlMs=0 cannot be replaced by MOA's periodic interval.
    let freshness = CatalogFreshness {
        discovery_fresh_until: Some(cache_deadline(60_000)),
        tools_fresh_until: Some(cache_deadline(0)),
    };

    assert!(
        freshness
            .effective_deadline()
            .is_some_and(|deadline| deadline <= std::time::Instant::now()),
        "ttlMs=0 must remain immediately expired"
    );
}
