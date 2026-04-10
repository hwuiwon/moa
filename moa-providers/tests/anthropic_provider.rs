use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use moa_core::{CompletionContent, CompletionRequest, LLMProvider, StopReason};
use moa_providers::AnthropicProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

const MODEL: &str = "claude-sonnet-4-6";

#[tokio::test]
async fn provider_streams_tokens_incrementally() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();

        socket
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "cache-control: no-cache\r\n",
                    "connection: close\r\n\r\n",
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":8}}}\n\n",
                    "event: content_block_start\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.flush().await.unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        socket
            .write_all(
                concat!(
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
                    "event: content_block_stop\n",
                    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: message_delta\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = AnthropicProvider::new("test-key", MODEL)
        .unwrap()
        .with_messages_url(format!("http://{address}/v1/messages"))
        .with_max_retries(0);

    let mut stream = provider
        .complete(CompletionRequest::simple("hello"))
        .await
        .unwrap();
    let first_block = timeout(Duration::from_millis(50), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(first_block, CompletionContent::Text("Hel".to_string()));

    let response = stream.collect().await.unwrap();
    assert_eq!(response.text, "Hello");
    assert_eq!(response.output_tokens, 5);

    server.abort();
}

#[tokio::test]
async fn provider_retries_after_rate_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_task = Arc::clone(&request_count);

    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            let current = request_count_task.fetch_add(1, Ordering::SeqCst);

            if current == 0 {
                socket
                    .write_all(
                        concat!(
                            "HTTP/1.1 429 Too Many Requests\r\n",
                            "content-length: 10\r\n",
                            "connection: close\r\n\r\n",
                            "rate limit"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                continue;
            }

            socket
                .write_all(
                    concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "content-type: text/event-stream\r\n",
                        "cache-control: no-cache\r\n",
                        "connection: close\r\n\r\n",
                        "event: message_start\n",
                        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":5}}}\n\n",
                        "event: content_block_start\n",
                        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                        "event: content_block_delta\n",
                        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
                        "event: content_block_stop\n",
                        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                        "event: message_delta\n",
                        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
                        "event: message_stop\n",
                        "data: {\"type\":\"message_stop\"}\n\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            break;
        }
    });

    let provider = AnthropicProvider::new("test-key", MODEL)
        .unwrap()
        .with_messages_url(format!("http://{address}/v1/messages"))
        .with_max_retries(3);

    let response = provider
        .complete(CompletionRequest::simple("hello"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "ok");
    assert_eq!(request_count.load(Ordering::SeqCst), 2);

    server.abort();
}

#[tokio::test]
async fn provider_streams_tool_use_blocks() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();

        socket
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "cache-control: no-cache\r\n",
                    "connection: close\r\n\r\n",
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":8}}}\n\n",
                    "event: content_block_start\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n\n",
                    "event: content_block_stop\n",
                    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                    "event: message_delta\n",
                    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = AnthropicProvider::new("test-key", MODEL)
        .unwrap()
        .with_messages_url(format!("http://{address}/v1/messages"))
        .with_max_retries(0);

    let mut stream = provider
        .complete(CompletionRequest::simple("show me cwd"))
        .await
        .unwrap();
    let first_block = stream.next().await.unwrap().unwrap();
    assert_eq!(
        first_block,
        CompletionContent::ToolCall(moa_core::ToolInvocation {
            id: Some("toolu_1".to_string()),
            name: "bash".to_string(),
            input: serde_json::json!({ "cmd": "pwd" }),
        })
    );

    let response = stream.collect().await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);

    server.abort();
}

#[tokio::test]
async fn provider_returns_error_after_partial_output_without_retrying() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_count = Arc::new(AtomicUsize::new(0));
    let request_count_task = Arc::clone(&request_count);

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();
        request_count_task.fetch_add(1, Ordering::SeqCst);

        socket
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "cache-control: no-cache\r\n",
                    "connection: close\r\n\r\n",
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":8}}}\n\n",
                    "event: content_block_start\n",
                    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
                    "event: error\n",
                    "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"rate limit\"}}\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = AnthropicProvider::new("test-key", MODEL)
        .unwrap()
        .with_messages_url(format!("http://{address}/v1/messages"))
        .with_max_retries(3);

    let mut stream = provider
        .complete(CompletionRequest::simple("hello"))
        .await
        .unwrap();
    let first_block = timeout(Duration::from_millis(50), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first_block, CompletionContent::Text("Hel".to_string()));

    let error = stream.collect().await.unwrap_err();
    assert!(error.to_string().contains("rate limit"));
    assert_eq!(request_count.load(Ordering::SeqCst), 1);

    server.abort();
}
