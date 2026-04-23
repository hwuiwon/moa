use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use moa_core::{
    CompletionContent, CompletionRequest, ContextMessage, LLMProvider, StopReason, ToolContent,
};
use moa_providers::OpenAIProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

const MODEL: &str = "gpt-5.4";

#[tokio::test]
async fn openai_provider_translates_requests_to_responses_api() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("POST /v1/responses"));
        assert!(request.contains("\"model\":\"gpt-5.4\""));
        assert!(request.contains("\"instructions\":\"Follow the rules.\""));
        assert!(request.contains("\"role\":\"user\""));
        assert!(request.contains("\"content\":\"hello\""));
        assert!(request.contains("\"type\":\"function\""));
        assert!(request.contains("\"name\":\"file_read\""));
        assert!(request.contains("\"strict\":true"));
        assert!(
            request.contains("\"required\":[\"path\",\"encoding\"]")
                || request.contains("\"required\":[\"encoding\",\"path\"]")
        );
        assert!(request.contains("\"encoding\":{\"type\":[\"string\",\"null\"]}"));
        assert!(request.contains("\"additionalProperties\":false"));
        assert!(!request.contains("\"minLength\""));
        assert!(
            request.contains("\"tool_choice\":\"auto\"")
                || request.contains("\"tool_choice\":{\"mode\":\"auto\"")
        );

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 1).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);
    let request = CompletionRequest {
        model: None,
        messages: vec![
            moa_core::ContextMessage::system("Follow the rules."),
            moa_core::ContextMessage::user("hello"),
        ],
        tools: vec![serde_json::json!({
            "name": "file_read",
            "description": "Read a file",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "minLength": 1 },
                    "encoding": { "type": "string" }
                },
                "required": ["path"]
            }
        })],
        max_output_tokens: Some(128),
        temperature: Some(0.2),
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let response = provider
        .complete(request)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_serializes_tool_result_messages_as_function_call_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"type\":\"function_call_output\""));
        assert!(request.contains("\"call_id\":\"fc_123\""));
        assert!(request.contains("\"text\":\"summary\""));
        assert!(request.contains("\"text\":\"{\\\"path\\\":\\\"notes/today.md\\\"}\""));

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 1).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);
    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::tool_result(
            "fc_123",
            "fallback",
            Some(vec![
                ToolContent::Text {
                    text: "summary".to_string(),
                },
                ToolContent::Json {
                    data: serde_json::json!({ "path": "notes/today.md" }),
                },
            ]),
        )],
        tools: Vec::new(),
        max_output_tokens: Some(128),
        temperature: Some(0.2),
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let response = provider
        .complete(request)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_serializes_assistant_tool_calls_as_function_call_items() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"type\":\"function_call\""));
        assert!(request.contains("\"call_id\":\"fc_history_1\""));
        assert!(request.contains("\"name\":\"file_write\""));
        assert!(request.contains("\"arguments\":\"{\\\"path\\\":\\\"live/openai.txt\\\"}\""));

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 1).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);
    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::assistant_tool_call(
            moa_core::ToolInvocation {
                id: Some("fc_history_1".to_string()),
                name: "file_write".to_string(),
                input: serde_json::json!({ "path": "live/openai.txt" }),
            },
            "<tool_call name=\"file_write\">{\"path\":\"live/openai.txt\"}</tool_call>",
        )],
        tools: Vec::new(),
        max_output_tokens: Some(128),
        temperature: Some(0.2),
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let response = provider
        .complete(request)
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_includes_native_web_search_when_enabled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"type\":\"web_search\""));

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 0).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);

    let response = provider
        .complete(CompletionRequest::simple(
            "What happened in the news today?",
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_omits_native_web_search_when_disabled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(!request.contains("\"type\":\"web_search\""));

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 0).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_web_search_enabled(false)
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);

    let response = provider
        .complete(CompletionRequest::simple(
            "What happened in the news today?",
        ))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_streams_tool_calls_from_responses_events() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();

        socket
            .write_all(tool_call_stream(MODEL).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);
    let request = CompletionRequest {
        model: None,
        messages: vec![moa_core::ContextMessage::user("show me cwd")],
        tools: vec![serde_json::json!({
            "name": "bash",
            "description": "Run a command",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }
        })],
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let mut stream = provider.complete(request).await.unwrap();
    let first_block = timeout(Duration::from_millis(50), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(
        first_block,
        CompletionContent::ToolCall(moa_core::ToolCallContent {
            invocation: moa_core::ToolInvocation {
                id: Some("fc_1".to_string()),
                name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "pwd" }),
            },
            provider_metadata: None,
        })
    );

    let response = stream.collect().await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);

    server.abort();
}

#[tokio::test]
async fn openai_provider_retries_after_rate_limit() {
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
                let body = "{\"error\":{\"message\":\"rate limit\",\"type\":\"rate_limit_error\",\"code\":\"rate_limit_exceeded\"}}";
                socket
                    .write_all(
                        format!(
                            concat!(
                                "HTTP/1.1 429 Too Many Requests\r\n",
                                "content-type: application/json\r\n",
                                "content-length: {}\r\n",
                                "connection: close\r\n\r\n",
                                "{}"
                            ),
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                continue;
            }

            socket
                .write_all(simple_text_stream("ok", MODEL, 5, 2, 0).as_bytes())
                .await
                .unwrap();
            socket.flush().await.unwrap();
            break;
        }
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
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
async fn openai_provider_drops_oversized_metadata_values() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 16384];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"request_id\":\"tiny\""));
        assert!(!request.contains("\"oversized\":"));

        socket
            .write_all(simple_text_stream("ok", MODEL, 8, 2, 0).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);
    let mut metadata = HashMap::new();
    metadata.insert("request_id".to_string(), serde_json::json!("tiny"));
    metadata.insert("oversized".to_string(), serde_json::json!("x".repeat(600)));

    let response = provider
        .complete(CompletionRequest {
            model: None,
            messages: vec![ContextMessage::user("hello")],
            tools: vec![],
            max_output_tokens: Some(32),
            temperature: None,
            response_format: None,
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            metadata,
        })
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "ok");

    server.abort();
}

#[tokio::test]
async fn openai_provider_does_not_retry_after_partial_stream_output() {
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
            .write_all(partial_text_then_error_stream(MODEL, "Hel", "rate limit").as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
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

#[tokio::test]
async fn openai_provider_streams_parallel_tool_calls_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();

        socket
            .write_all(parallel_tool_call_stream(MODEL).as_bytes())
            .await
            .unwrap();
        socket.flush().await.unwrap();
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .unwrap()
        .with_api_base(format!("http://{address}/v1"))
        .unwrap()
        .with_max_retries(0);

    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::user("list cwd and whoami")],
        tools: vec![
            serde_json::json!({
                "name": "bash",
                "description": "Run a command",
                "input_schema": {
                    "type": "object",
                    "properties": { "cmd": { "type": "string" } },
                    "required": ["cmd"]
                }
            }),
            serde_json::json!({
                "name": "file_read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }),
        ],
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let mut stream = provider.complete(request).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    let second = stream.next().await.unwrap().unwrap();

    assert_eq!(
        first,
        CompletionContent::ToolCall(moa_core::ToolCallContent {
            invocation: moa_core::ToolInvocation {
                id: Some("fc_1".to_string()),
                name: "bash".to_string(),
                input: serde_json::json!({ "cmd": "pwd" }),
            },
            provider_metadata: None,
        })
    );
    assert_eq!(
        second,
        CompletionContent::ToolCall(moa_core::ToolCallContent {
            invocation: moa_core::ToolInvocation {
                id: Some("fc_2".to_string()),
                name: "file_read".to_string(),
                input: serde_json::json!({ "path": "Cargo.toml" }),
            },
            provider_metadata: None,
        })
    );

    let response = stream.collect().await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.content.len(), 2);

    server.abort();
}

#[tokio::test]
async fn openai_provider_rejects_system_only_requests() {
    let provider = OpenAIProvider::new("test-key", MODEL).unwrap();
    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::system("Only a system prompt")],
        tools: vec![],
        max_output_tokens: None,
        temperature: None,
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: Default::default(),
    };

    let error = provider.complete(request).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("at least one non-system message")
    );
}

fn simple_text_stream(
    text: &str,
    model: &str,
    input_tokens: usize,
    output_tokens: usize,
    cached_tokens: usize,
) -> String {
    let events = [
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "model": model,
                "output": [],
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": text,
            "logprobs": null
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": model,
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "status": "completed",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                        "logprobs": null
                    }]
                }],
                "status": "completed",
                "usage": {
                    "input_tokens": input_tokens,
                    "input_tokens_details": {
                        "cached_tokens": cached_tokens
                    },
                    "output_tokens": output_tokens,
                    "output_tokens_details": {
                        "reasoning_tokens": 0
                    },
                    "total_tokens": input_tokens + output_tokens
                }
            }
        }),
    ];

    sse_response(events.into_iter().collect())
}

fn tool_call_stream(model: &str) -> String {
    let events = [
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "model": model,
                "output": [],
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "bash",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 2,
            "item_id": "fc_1",
            "output_index": 0,
            "arguments": "{\"cmd\":\"pwd\"}",
            "name": "bash"
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 3,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": model,
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "bash",
                    "arguments": "{\"cmd\":\"pwd\"}",
                    "status": "completed"
                }],
                "status": "completed",
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": {
                        "cached_tokens": 0
                    },
                    "output_tokens": 4,
                    "output_tokens_details": {
                        "reasoning_tokens": 0
                    },
                    "total_tokens": 14
                }
            }
        }),
    ];

    sse_response(events.into_iter().collect())
}

fn parallel_tool_call_stream(model: &str) -> String {
    let events = [
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "model": model,
                "output": [],
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "bash",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc_2",
                "call_id": "call_2",
                "name": "file_read",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 3,
            "item_id": "fc_1",
            "output_index": 0,
            "arguments": "{\"cmd\":\"pwd\"}",
            "name": "bash"
        }),
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 4,
            "item_id": "fc_2",
            "output_index": 1,
            "arguments": "{\"path\":\"Cargo.toml\"}",
            "name": "file_read"
        }),
        serde_json::json!({
            "type": "response.completed",
            "sequence_number": 5,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": model,
                "output": [
                    {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "bash",
                        "arguments": "{\"cmd\":\"pwd\"}",
                        "status": "completed"
                    },
                    {
                        "type": "function_call",
                        "id": "fc_2",
                        "call_id": "call_2",
                        "name": "file_read",
                        "arguments": "{\"path\":\"Cargo.toml\"}",
                        "status": "completed"
                    }
                ],
                "status": "completed",
                "usage": {
                    "input_tokens": 12,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 8,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 20
                }
            }
        }),
    ];

    sse_response(events.into_iter().collect())
}

fn partial_text_then_error_stream(model: &str, delta: &str, message: &str) -> String {
    let events = [
        serde_json::json!({
            "type": "response.created",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "model": model,
                "output": [],
                "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": delta,
            "logprobs": null
        }),
        serde_json::json!({
            "type": "response.error",
            "sequence_number": 2,
            "code": "rate_limit_exceeded",
            "message": message,
            "param": null
        }),
    ];

    sse_response(events.into_iter().collect())
}

fn sse_response(events: Vec<serde_json::Value>) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str("data: ");
        body.push_str(&event.to_string());
        body.push_str("\n\n");
    }

    format!(
        concat!(
            "HTTP/1.1 200 OK\r\n",
            "content-type: text/event-stream\r\n",
            "cache-control: no-cache\r\n",
            "connection: close\r\n\r\n",
            "{body}"
        ),
        body = body
    )
}
