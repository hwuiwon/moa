//! Gemini adapter unit tests.

use moa_core::{
    traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::JsonResponseFormat,
    types::completion::ProviderToolCallMetadata, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::context::ContextMessage, types::identifiers::ModelId,
    types::tools::ToolContent,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{
    GeminiProvider, GeminiUsageMetadata, build_request_body, canonical_model_id,
    capabilities_for_model, thinking_config_for_model, token_usage_from_gemini_usage,
};

fn sse_stream(frames: &[Value]) -> String {
    let mut stream = String::new();
    for frame in frames {
        stream.push_str("data: ");
        stream.push_str(&frame.to_string());
        stream.push_str("\n\n");
    }
    stream
}

#[test]
fn gemini_request_sets_structured_output_schema() {
    let mut request = CompletionRequest::new("Return structured data.");
    request.response_format = Some(JsonResponseFormat::strict_json_schema(
        "test_payload",
        "Test payload.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "answer": { "type": "string" }
            },
            "required": ["answer"]
        }),
    ));

    let body = build_request_body(&request, "gemini-3-flash-preview", "medium", &[])
        .expect("request should build");

    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseSchema"]["required"],
        json!(["answer"])
    );
}

#[tokio::test]
async fn gemini_provider_serializes_system_messages_and_tools() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 16384];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(
            request.contains(
                "POST /v1beta/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
            )
        );
        assert!(
            request
                .contains("\"systemInstruction\":{\"parts\":[{\"text\":\"Follow the rules.\"}]}")
        );
        assert!(request.contains("\"role\":\"user\""));
        assert!(request.contains("\"text\":\"hello\""));
        assert!(request.contains("\"functionDeclarations\":[{"));
        assert!(request.contains("\"name\":\"file_read\""));
        assert!(request.contains("\"description\":\"Read a file\""));
        assert!(!request.contains("\"additionalProperties\":false"));
        assert!(!request.contains("\"google_search\":{}"));
        assert!(request.contains("\"thinkingLevel\":\"medium\""));

        let body = sse_stream(&[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "ok" }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 2,
                "cachedContentTokenCount": 1
            },
            "modelVersion": "gemini-3-flash-preview"
        })]);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let provider = GeminiProvider::new("test-key", "gemini-3-flash-preview")
        .unwrap()
        .with_api_base(format!("http://{address}/v1beta"))
        .with_max_retries(0);
    let response = provider
        .complete(CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("Follow the rules."),
                ContextMessage::user("hello"),
            ],
            tools: vec![json!({
                "name": "file_read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "additionalProperties": false,
                    "required": ["path"]
                }
            })],
            max_output_tokens: Some(1024),
            temperature: Some(0.2),
            response_format: None,
            metadata: Default::default(),
        })
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "ok");
    assert_eq!(response.model, ModelId::new("gemini-3-flash-preview"));
    server.abort();
}

#[test]
fn token_usage_from_gemini_usage_splits_cached_prompt_tokens() {
    let usage = GeminiUsageMetadata {
        prompt_token_count: Some(2048),
        candidates_token_count: Some(512),
        cached_content_token_count: Some(1536),
    };

    let token_usage = token_usage_from_gemini_usage(&usage);
    assert_eq!(token_usage.input_tokens_uncached, 512);
    assert_eq!(token_usage.input_tokens_cache_write, 0);
    assert_eq!(token_usage.input_tokens_cache_read, 1536);
    assert_eq!(token_usage.output_tokens, 512);
}

#[test]
fn gemini_preview_model_ids_pass_through_unchanged() {
    assert_eq!(
        canonical_model_id("gemini-3.1-pro-preview").unwrap(),
        "gemini-3.1-pro-preview"
    );
    assert_eq!(
        canonical_model_id("gemini-3-pro-preview").unwrap(),
        "gemini-3-pro-preview"
    );
    assert_eq!(
        canonical_model_id("gemini-3-flash-preview").unwrap(),
        "gemini-3-flash-preview"
    );
}

#[test]
fn unsupported_gemini_2_models_are_rejected() {
    let unsupported = format!("gemini-{}.{}-flash", 2, 5);
    let error = canonical_model_id(&unsupported).expect_err("unsupported model should be rejected");
    assert!(error.to_string().contains("Gemini 2 models"));
}

#[test]
fn uncatalogued_gemini_models_are_rejected() {
    let error = canonical_model_id("gemini-3-unpriced-experimental")
        .expect_err("uncatalogued Gemini model should be rejected");
    assert!(
        error
            .to_string()
            .contains("unsupported Google Gemini model")
    );
    assert!(capabilities_for_model("gemini-3-unpriced-experimental").is_err());
}

#[test]
fn gemini_3_flash_preview_uses_documented_price_envelope() {
    let capabilities = capabilities_for_model("gemini-3-flash-preview").unwrap();
    assert_eq!(capabilities.context_window, 1_048_576);
    assert_eq!(capabilities.max_output, 65_536);
    assert_eq!(capabilities.pricing.input_per_mtok, 0.5);
    assert_eq!(capabilities.pricing.output_per_mtok, 3.0);
}

#[test]
fn gemini_3_pro_maps_medium_reasoning_to_medium_thinking_level() {
    let thinking = thinking_config_for_model("gemini-3.1-pro-preview", "medium")
        .unwrap()
        .expect("thinking config");
    assert_eq!(thinking["thinkingLevel"], "medium");
}

#[test]
fn gemini_3_flash_uses_minimal_thinking_for_tiny_output_caps() {
    let mut request = CompletionRequest::simple("Describe Rome briefly.");
    request.max_output_tokens = Some(16);

    let body = build_request_body(&request, "gemini-3-flash-preview", "medium", &[])
        .expect("request body");

    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "minimal"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 16);
}

#[test]
fn gemini_request_body_keeps_cacheable_prompt_inline() {
    // Pins: Gemini adapter relies on implicit caching and does not create cachedContent references.
    let request = CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system("cached rules"),
            ContextMessage::user("prefix ".repeat(300)),
            ContextMessage::system("late rule"),
            ContextMessage::user("call the tool"),
        ],
        tools: vec![json!({
            "name": "file_write",
            "description": "Write a file",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }
        })],
        max_output_tokens: Some(256),
        temperature: None,
        response_format: None,
        metadata: Default::default(),
    };

    let body = build_request_body(&request, "gemini-3-flash-preview", "medium", &[])
        .expect("request body should build");

    assert!(body.get("cachedContent").is_none());
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "cached rules"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["text"], "late rule",
        "late system messages should stay in the dynamic contents stream"
    );
}

#[tokio::test]
async fn gemini_provider_groups_tool_history_and_preserves_thought_signatures() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 16384];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"functionCall\":{"));
        assert!(request.contains("\"name\":\"file_write\""));
        assert!(request.contains("\"id\":\"fc_1\""));
        assert!(request.contains("\"args\":{\"path\":\"notes/today.md\"}"));
        assert!(request.contains("\"thoughtSignature\":\"sig_fc_1\""));
        assert!(request.contains("\"functionResponse\":{"));
        assert!(request.contains("\"name\":\"file_write\""));
        assert!(request.contains("\"id\":\"fc_1\""));
        assert!(request.contains("\"result\":{\"path\":\"notes/today.md\"}"));

        let body = sse_stream(&[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "done" }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 2
            },
            "modelVersion": "gemini-3-flash-preview"
        })]);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let provider = GeminiProvider::new("test-key", "gemini-3-flash-preview")
        .unwrap()
        .with_api_base(format!("http://{address}/v1beta"))
        .with_max_retries(0);
    let response = provider
        .complete(CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::user("write the file"),
                ContextMessage::assistant_tool_call_with_thought_signature(
                    ToolInvocation {
                        id: Some("fc_1".to_string()),
                        name: "file_write".to_string(),
                        input: json!({ "path": "notes/today.md" }),
                    },
                    "<tool_call />",
                    Some("sig_fc_1"),
                ),
                ContextMessage::tool_result(
                    "fc_1",
                    "ok",
                    Some(vec![ToolContent::Json {
                        data: json!({ "path": "notes/today.md" }),
                    }]),
                ),
            ],
            tools: Vec::new(),
            max_output_tokens: Some(1024),
            temperature: None,
            response_format: None,
            metadata: Default::default(),
        })
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "done");
    server.abort();
}

#[tokio::test]
async fn gemini_provider_serializes_google_search_without_functions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 16384];
        let read = socket.read(&mut buffer).await.unwrap();
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        assert!(request.contains("\"google_search\":{}"));
        assert!(!request.contains("\"functionDeclarations\""));

        let body = sse_stream(&[json!({
            "candidates": [{
                "content": {
                    "parts": [{ "text": "headline" }]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 2
            },
            "modelVersion": "gemini-3-flash-preview"
        })]);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let provider = GeminiProvider::new("test-key", "gemini-3-flash-preview")
        .unwrap()
        .with_api_base(format!("http://{address}/v1beta"))
        .with_max_retries(0);
    let response = provider
        .complete(CompletionRequest::simple("latest news"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "headline");
    server.abort();
}

#[tokio::test]
async fn gemini_provider_streams_tool_calls_and_google_search_updates() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 4096];
        let _ = socket.read(&mut buffer).await.unwrap();

        let body = sse_stream(&[
            json!({
                "candidates": [{
                    "content": {
                        "parts": [{
                            "functionCall": {
                                "id": "fc_stream_1",
                                "name": "emit_token",
                                "args": { "token": "LIVE" }
                            },
                            "thoughtSignature": "sig_stream_1"
                        }]
                    }
                }]
            }),
            json!({
                "candidates": [{
                    "content": {
                        "parts": [{ "text": "headline" }]
                    },
                    "groundingMetadata": {
                        "webSearchQueries": ["latest headline"]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 11,
                    "candidatesTokenCount": 3
                },
                "modelVersion": "gemini-3-flash-preview"
            }),
        ]);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });

    let provider = GeminiProvider::new("test-key", "gemini-3-flash-preview")
        .unwrap()
        .with_api_base(format!("http://{address}/v1beta"))
        .with_max_retries(0);
    let response = provider
        .complete(CompletionRequest::simple("latest news"))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();

    assert_eq!(response.text, "headline");
    assert!(response.content.iter().any(|content| {
        matches!(
            content,
            CompletionContent::ToolCall(ToolCallContent {
                invocation: ToolInvocation { id, name, .. },
                provider_metadata: Some(ProviderToolCallMetadata::Gemini { thought_signature }),
            }) if id.as_deref() == Some("fc_stream_1")
                && name == "emit_token"
                && thought_signature == "sig_stream_1"
        )
    }));
    assert!(response.content.iter().any(|content| {
        matches!(
            content,
            CompletionContent::ProviderToolResult { tool_name, .. } if tool_name == "web_search"
        )
    }));
    server.abort();
}
