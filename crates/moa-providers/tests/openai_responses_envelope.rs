//! Snapshot coverage for OpenAI Responses request envelopes.

use std::collections::HashMap;

use moa_core::transcript::ProviderEvent;
use moa_core::{
    traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::StopReason,
    types::completion::TokenUsage, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::context::ContextMessage, types::identifiers::ModelId,
};
use moa_providers::{OpenAIProvider, debug_build_openai_request_body};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const MODEL: &str = "gpt-5.4-mini";
const PREVIOUS_RESPONSE_ID_METADATA_KEY: &str = "_moa.openai.previous_response_id";
const TOOL_CHOICE_METADATA_KEY: &str = "_moa.openai.tool_choice";

#[test]
fn openai_responses_request_serializes_with_separate_instructions_and_input_fields() {
    let body = openai_body(&base_request());

    snapshot_json(
        "openai_responses_envelope__openai_responses_request_serializes_with_separate_instructions_and_input_fields",
        &body,
    );
}

#[test]
fn openai_responses_request_with_tool_choice_required_serializes_correctly() {
    let mut request = base_request();
    request
        .metadata
        .insert(TOOL_CHOICE_METADATA_KEY.to_string(), json!("required"));
    let body = openai_body(&request);

    assert_eq!(body["tool_choice"], "required");
    snapshot_json(
        "openai_responses_envelope__openai_responses_request_with_tool_choice_required_serializes_correctly",
        &body,
    );
}

#[test]
fn openai_responses_request_with_previous_response_id_chains_state_correctly() {
    let mut request = base_request();
    request.metadata.insert(
        PREVIOUS_RESPONSE_ID_METADATA_KEY.to_string(),
        json!("resp_previous_123"),
    );
    let body = openai_body(&request);

    assert_eq!(body["previous_response_id"], "resp_previous_123");
    assert!(body.get("metadata").is_none());
    snapshot_json(
        "openai_responses_envelope__openai_responses_request_with_previous_response_id_chains_state_correctly",
        &body,
    );
}

#[test]
fn openai_responses_request_with_function_tools_includes_strict_mode_when_configured() {
    let body = openai_body(&base_request());

    assert_eq!(body["tools"][0]["strict"], true);
    snapshot_json(
        "openai_responses_envelope__openai_responses_request_with_function_tools_includes_strict_mode_when_configured",
        &body,
    );
}

#[tokio::test]
async fn openai_responses_streaming_response_chunks_parse_into_provider_events() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local OpenAI Responses fixture server");
    let address = listener
        .local_addr()
        .expect("read local fixture server address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener
            .accept()
            .await
            .expect("accept OpenAI Responses fixture connection");
        let mut buffer = vec![0_u8; 8192];
        let _ = socket
            .read(&mut buffer)
            .await
            .expect("read OpenAI Responses request");
        let response = recorded_responses_sse_fixture(MODEL);
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write OpenAI Responses SSE fixture");
        socket.flush().await.expect("flush SSE fixture");
    });

    let provider = OpenAIProvider::new("test-key", MODEL)
        .expect("create OpenAI provider")
        .with_api_base(format!("http://{address}/v1"))
        .expect("override OpenAI API base")
        .with_max_retries(0);
    let mut stream = provider
        .complete(CompletionRequest::simple("show me the workspace root"))
        .await
        .expect("start OpenAI Responses stream");

    let mut events = Vec::new();
    while let Some(block) = stream.next().await {
        events.push(provider_event_from_content(block.expect("stream block")));
    }
    let response = stream.collect().await.expect("collect streamed response");
    events.push(ProviderEvent::Usage {
        usage: response.usage,
    });
    events.push(ProviderEvent::Terminal {
        stop_reason: response.stop_reason,
    });

    assert_eq!(
        events,
        vec![
            ProviderEvent::TextDelta {
                text: "Working ".to_string(),
            },
            ProviderEvent::ToolCall {
                call: ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("fc_1".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "pwd" }),
                    },
                    provider_metadata: None,
                },
            },
            ProviderEvent::Usage {
                usage: TokenUsage {
                    input_tokens_uncached: 9,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 3,
                    output_tokens: 5,
                },
            },
            ProviderEvent::Terminal {
                stop_reason: StopReason::ToolUse,
            },
        ]
    );

    server.abort();
}

fn openai_body(request: &CompletionRequest) -> Value {
    debug_build_openai_request_body(request, false).expect("OpenAI Responses body should build")
}

fn snapshot_json(name: &str, value: &Value) {
    insta::with_settings!({ prepend_module_to_snapshot => false, sort_maps => true }, {
        insta::assert_json_snapshot!(name, value);
    });
}

fn base_request() -> CompletionRequest {
    CompletionRequest {
        model: Some(ModelId::new(MODEL)),
        messages: vec![
            ContextMessage::system("Follow the workspace policy."),
            ContextMessage::system("Prefer read-only inspection before edits."),
            ContextMessage::user("Inspect Cargo.toml and report the provider crate name."),
        ],
        tools: vec![file_read_tool()],
        max_output_tokens: Some(256),
        temperature: Some(0.0),
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    }
}

fn file_read_tool() -> Value {
    json!({
        "name": "file_read",
        "description": "Read a UTF-8 file from the workspace.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path"
                }
            },
            "required": ["path"]
        }
    })
}

fn provider_event_from_content(content: CompletionContent) -> ProviderEvent {
    match content {
        CompletionContent::Text(text) => ProviderEvent::TextDelta { text },
        CompletionContent::ToolCall(call) => ProviderEvent::ToolCall { call },
        CompletionContent::ProviderToolResult { tool_name, summary } => ProviderEvent::TextDelta {
            text: format!("{tool_name}: {summary}"),
        },
    }
}

fn recorded_responses_sse_fixture(model: &str) -> String {
    let events = [
        json!({
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
        json!({
            "type": "response.output_text.delta",
            "sequence_number": 1,
            "item_id": "msg_1",
            "output_index": 0,
            "content_index": 0,
            "delta": "Working ",
            "logprobs": null
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 1,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "bash",
                "arguments": "",
                "status": "in_progress"
            }
        }),
        json!({
            "type": "response.function_call_arguments.done",
            "sequence_number": 3,
            "item_id": "fc_1",
            "output_index": 1,
            "arguments": "{\"cmd\":\"pwd\"}",
            "name": "bash"
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 4,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 1,
                "completed_at": 2,
                "model": model,
                "output": [
                    {
                        "type": "message",
                        "id": "msg_1",
                        "role": "assistant",
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": "Working ",
                            "annotations": [],
                            "logprobs": null
                        }]
                    },
                    {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "bash",
                        "arguments": "{\"cmd\":\"pwd\"}",
                        "status": "completed"
                    }
                ],
                "status": "completed",
                "usage": {
                    "input_tokens": 12,
                    "input_tokens_details": {
                        "cached_tokens": 3
                    },
                    "output_tokens": 5,
                    "output_tokens_details": {
                        "reasoning_tokens": 0
                    },
                    "total_tokens": 17
                }
            }
        }),
    ];

    sse_response(events.into_iter().collect())
}

fn sse_response(events: Vec<Value>) -> String {
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
