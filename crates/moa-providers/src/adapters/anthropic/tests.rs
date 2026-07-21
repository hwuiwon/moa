//! Anthropic adapter unit tests.

use std::time::Instant;

use eventsource_stream::Eventsource;
use futures_util::stream;
use moa_config::ProviderStreamTimeoutConfig;
use moa_core::{
    traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::JsonResponseFormat,
    types::completion::StopReason, types::completion::ToolInvocation,
    types::context::ContextMessage, types::identifiers::ModelId, types::tools::ToolContent,
};
use serde_json::json;
use tokio::sync::mpsc;

use super::{
    AnthropicProvider, MODEL_HAIKU_4_5, MODEL_OPUS_4_6, MODEL_SONNET_4_6, anthropic_content_blocks,
    anthropic_message, anthropic_tool_from_schema, build_request_body, canonical_model_id,
    capabilities_for_model, consume_sse_events,
};
use crate::core::instrumentation::LLMSpanRecorder;

#[test]
fn completion_request_serializes_to_anthropic_format() {
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::system("System one"),
            ContextMessage::system("System two"),
            ContextMessage::user("Hello"),
            ContextMessage::assistant("Hi"),
        ],
        tools: vec![json!({
            "name": "bash",
            "description": "Run shell commands",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }
        })],
        max_output_tokens: Some(512),
        temperature: Some(0.2),
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
        &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
        true,
    )
    .unwrap();

    assert_eq!(body["model"], MODEL_SONNET_4_6);
    assert_eq!(body["system"][0]["text"], "System one");
    assert_eq!(body["system"][1]["text"], "System two");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "Hello");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["tools"][0]["name"], "bash");
    assert_eq!(body["tools"][0]["input_schema"]["required"], json!(["cmd"]));
    assert_eq!(body["stream"], true);
}

#[test]
fn completion_request_sets_structured_output_config() {
    let mut request = CompletionRequest::new("Return structured data.");
    let canonical_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "intent": {
                "type": "string",
                "enum": ["coding", "research", "unknown"]
            },
            "confidence": {
                "oneOf": [
                    {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100
                    },
                    { "type": "null" }
                ]
            }
        },
        "required": ["intent", "confidence"]
    });
    request.response_format = Some(JsonResponseFormat::strict_json_schema(
        "query_rewrite_result",
        "Query rewrite result.",
        canonical_schema.clone(),
    ));

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_HAIKU_4_5).expect("valid model"),
        &capabilities_for_model(MODEL_HAIKU_4_5).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        body["output_config"]["format"]["schema"]["properties"]["intent"]["enum"],
        json!(["coding", "research", "unknown"])
    );
    assert_eq!(
        body["output_config"]["format"]["schema"]["properties"]["confidence"],
        json!({
            "anyOf": [
                { "type": "integer" },
                { "type": "null" }
            ]
        })
    );
    assert_eq!(
        request
            .response_format
            .as_ref()
            .expect("canonical response format should remain present")
            .schema,
        canonical_schema
    );
}

#[test]
fn completion_request_keeps_late_system_messages_out_of_stable_system_prefix() {
    // Pins: only leading system messages are lifted into Anthropic's system array.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::system("Stable rules"),
            ContextMessage::user("First task"),
            ContextMessage::system("Dynamic security reminder"),
            ContextMessage::user("Second task"),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    assert_eq!(
        body["system"].as_array().expect("system array").len(),
        1,
        "late system messages should not enter the stable Anthropic system array"
    );
    assert_eq!(body["system"][0]["text"], "Stable rules");
    assert_eq!(body["messages"][0]["content"], "First task");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "Dynamic security reminder");
    assert_eq!(body["messages"][2]["content"], "Second task");
}

#[test]
fn completion_request_enables_top_level_cache_control_for_large_prompt() {
    // Pins: Anthropic cache behavior is provider-owned at the stable prefix boundary.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::system("S".repeat(5_000)),
            ContextMessage::user("Hello"),
        ],
        tools: vec![json!({
            "name": "bash",
            "description": "Run shell commands",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }
        })],
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(
        !body["messages"].to_string().contains("cache_control"),
        "message blocks should not contain nested cache_control markers"
    );
}

#[test]
fn completion_request_marks_frozen_history_boundary_with_cache_control() {
    // Pins: when the context pipeline reports a frozen-history boundary, the
    // last built message under the boundary carries an ephemeral cache marker
    // so the replayed-history span cache-reads across turns; nothing after
    // the boundary is marked.
    let mut metadata = std::collections::HashMap::new();
    // Frozen region: system prefix + two replayed history messages (indexes
    // 0..3); index 3 onward (memory reminder + active user turn) is per-turn.
    metadata.insert(
        moa_core::types::completion::STABLE_HISTORY_END_METADATA_KEY.to_string(),
        json!(3),
    );
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::system("S".repeat(5_000)),
            ContextMessage::user("replayed turn one"),
            ContextMessage::assistant("replayed answer one"),
            ContextMessage::user("<memory-reminder>fresh retrieval</memory-reminder>"),
            ContextMessage::user("active turn"),
        ],
        tools: Vec::new(),
        native_web_search: Default::default(),
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        metadata,
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    let messages = body["messages"].as_array().expect("messages array");
    // Built message 1 = "replayed answer one" (last under the boundary).
    let boundary_blocks = messages[1]["content"]
        .as_array()
        .expect("boundary message content normalizes to blocks");
    assert_eq!(
        boundary_blocks.last().expect("last block")["cache_control"]["type"],
        "ephemeral"
    );
    assert!(
        !messages[2].to_string().contains("cache_control")
            && !messages[3].to_string().contains("cache_control"),
        "per-turn tail must stay unmarked"
    );
}

#[test]
fn completion_request_omits_top_level_cache_control_for_small_prompt() {
    // Pins: small Anthropic requests avoid sending cache_control when there is no cacheable prefix.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![ContextMessage::user("Hello")],
        tools: Vec::new(),
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    assert!(body.get("cache_control").is_none());
}

#[test]
fn completion_request_counts_tool_tokens_toward_automatic_cache_control() {
    // Pins: large tool schemas can make an Anthropic request cacheable without explicit markers.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::system("brief"),
            ContextMessage::user("Hello"),
        ],
        tools: vec![json!({
            "name": "tool_a",
            "description": "A".repeat(6_000),
            "input_schema": { "type": "object" }
        })],
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["tools"][0].get("cache_control").is_none());
}

#[test]
fn completion_request_includes_native_web_search_when_enabled() {
    let body = build_request_body(
        &CompletionRequest::simple("What happened in the news today?"),
        &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
        &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
        true,
    )
    .unwrap();

    let tools = body["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|tool| tool["type"] == "web_search_20250305" && tool["name"] == "web_search")
    );
}

#[test]
fn completion_request_omits_native_web_search_when_disabled() {
    let body = build_request_body(
        &CompletionRequest::simple("What happened in the news today?"),
        &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
        &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
        false,
    )
    .unwrap();

    assert!(body.get("tools").is_none());
}

#[test]
fn native_web_search_policy_disables_anthropic_native_tool_per_request() {
    // Pins: request-scoped planning policy narrows globally enabled Anthropic web search.
    let mut request = CompletionRequest::simple("Plan without web search.");
    request.native_web_search = moa_core::types::completion::NativeWebSearchPolicy::Disabled;
    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("capabilities"),
        true,
    )
    .expect("request");
    assert!(body.get("tools").is_none());
}

#[test]
fn anthropic_content_blocks_render_text_and_json_as_text_blocks() {
    // Pins: Anthropic content-block serialization is direct; tool-output
    // trust wrapping is applied by history compilation before provider routing.
    let blocks = anthropic_content_blocks(&[
        ToolContent::Text {
            text: "summary".to_string(),
        },
        ToolContent::Json {
            data: json!({"path": "notes/today.md"}),
        },
    ]);

    assert_eq!(
        blocks,
        json!([
            {
                "type": "text",
                "text": "summary",
            },
            {
                "type": "text",
                "text": "{\"path\":\"notes/today.md\"}",
            },
        ])
    );
}

#[test]
fn anthropic_message_wraps_tool_results_with_tool_use_id() {
    let message = anthropic_message(&ContextMessage::tool_result(
        "toolu_123",
        "fallback",
        Some(vec![ToolContent::Text {
            text: "hello".to_string(),
        }]),
    ))
    .unwrap();

    assert_eq!(message["role"], "user");
    assert_eq!(message["content"][0]["type"], "tool_result");
    assert_eq!(message["content"][0]["tool_use_id"], "toolu_123");
    assert_eq!(message["content"][0]["content"][0]["type"], "text");
    assert_eq!(message["content"][0]["content"][0]["text"], "hello");
}

#[test]
fn anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks() {
    let message = anthropic_message(&ContextMessage::assistant_tool_call(
        ToolInvocation {
            id: Some("toolu_234".to_string()),
            name: "file_write".to_string(),
            input: json!({ "path": "live/anthropic.txt" }),
        },
        "<tool_call name=\"file_write\">{\"path\":\"live/anthropic.txt\"}</tool_call>",
    ))
    .unwrap();

    assert_eq!(message["role"], "assistant");
    assert_eq!(message["content"][0]["type"], "tool_use");
    assert_eq!(message["content"][0]["id"], "toolu_234");
    assert_eq!(message["content"][0]["name"], "file_write");
    assert_eq!(message["content"][0]["input"]["path"], "live/anthropic.txt");
}

#[test]
fn completion_request_groups_adjacent_tool_exchange_for_anthropic_protocol() {
    // Pins: MOA records each tool call/result as separate history messages, but
    // Anthropic requires one assistant tool_use message followed immediately by
    // one user tool_result message for the whole exchange.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::user("delegate both tasks"),
            ContextMessage::assistant_tool_call(
                ToolInvocation {
                    id: Some("toolu_a".to_string()),
                    name: "spawn_worker".to_string(),
                    input: json!({ "task": "A" }),
                },
                "<tool_call name=\"spawn_worker\">{\"task\":\"A\"}</tool_call>",
            ),
            ContextMessage::assistant_tool_call(
                ToolInvocation {
                    id: Some("toolu_b".to_string()),
                    name: "spawn_worker".to_string(),
                    input: json!({ "task": "B" }),
                },
                "<tool_call name=\"spawn_worker\">{\"task\":\"B\"}</tool_call>",
            ),
            ContextMessage::tool_result("toolu_a", "worker A spawned", None),
            ContextMessage::tool_result("toolu_b", "worker B spawned", None),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"].as_array().expect("tool calls").len(),
        2
    );
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    assert_eq!(messages[1]["content"][0]["id"], "toolu_a");
    assert_eq!(messages[1]["content"][1]["type"], "tool_use");
    assert_eq!(messages[1]["content"][1]["id"], "toolu_b");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"]
            .as_array()
            .expect("tool results")
            .len(),
        2
    );
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_a");
    assert_eq!(messages[2]["content"][1]["type"], "tool_result");
    assert_eq!(messages[2]["content"][1]["tool_use_id"], "toolu_b");
}

#[test]
fn completion_request_replays_non_adjacent_tool_history_as_text() {
    // Pins: if worker/control-plane events sit between a provider tool_use and
    // its result, Anthropic native replay would be invalid. Keep the history as
    // plain text instead of sending an orphaned native tool_use/tool_result pair.
    let request = CompletionRequest {
        model: Some(ModelId::new(MODEL_SONNET_4_6)),
        messages: vec![
            ContextMessage::user("delegate one task"),
            ContextMessage::assistant_tool_call(
                ToolInvocation {
                    id: Some("toolu_worker".to_string()),
                    name: "spawn_worker".to_string(),
                    input: json!({ "task": "A" }),
                },
                "<tool_call name=\"spawn_worker\">{\"task\":\"A\"}</tool_call>",
            ),
            ContextMessage::system("<worker_status state=\"running\" />"),
            ContextMessage::tool_result("toolu_worker", "<tool_result id=\"spawned\" />", None),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(512),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: Default::default(),
    };

    let body = build_request_body(
        &request,
        &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
        &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
        false,
    )
    .expect("request should build");

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"],
        "<tool_call name=\"spawn_worker\">{\"task\":\"A\"}</tool_call>"
    );
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(
        messages[2]["content"],
        "<worker_status state=\"running\" />"
    );
    assert_eq!(messages[3]["role"], "user");
    assert_eq!(messages[3]["content"], "<tool_result id=\"spawned\" />");
    for message in messages {
        if let Some(blocks) = message["content"].as_array() {
            assert!(
                blocks
                    .iter()
                    .all(|block| block["type"] != "tool_use" && block["type"] != "tool_result"),
                "non-adjacent tool history must not emit native Anthropic tool blocks"
            );
        }
    }
}

#[test]
fn anthropic_tool_from_schema_moves_parameters_into_input_schema() {
    let tool = anthropic_tool_from_schema(&json!({
        "name": "memory_search",
        "description": "Search memory",
        "parameters": {
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"]
        }
    }));

    assert_eq!(tool["name"], "memory_search");
    assert_eq!(tool["input_schema"]["required"], json!(["query"]));
}

#[tokio::test]
async fn parses_recorded_sse_stream_into_content_blocks() {
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"ls\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let raw_stream = stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(sse.as_bytes().to_vec())]);
    let events = raw_stream.eventsource();
    let (tx, mut rx) = mpsc::channel(8);
    let request = CompletionRequest::new("Hello");
    let capabilities = capabilities_for_model(MODEL_SONNET_4_6).unwrap();
    let mut span_recorder = LLMSpanRecorder::new(
        "anthropic",
        MODEL_SONNET_4_6,
        &request,
        request.max_output_tokens,
        capabilities.pricing,
    );

    let response = consume_sse_events(
        events,
        tx,
        MODEL_SONNET_4_6.to_string(),
        Instant::now(),
        &mut span_recorder,
        ProviderStreamTimeoutConfig::default(),
    )
    .await
    .unwrap();

    let mut streamed_blocks = Vec::new();
    while let Some(block) = rx.recv().await {
        streamed_blocks.push(block.unwrap());
    }

    assert_eq!(streamed_blocks.len(), 3);
    assert_eq!(
        streamed_blocks[0],
        CompletionContent::Text("Hel".to_string())
    );
    assert_eq!(
        streamed_blocks[1],
        CompletionContent::Text("lo".to_string())
    );
    match &streamed_blocks[2] {
        CompletionContent::ToolCall(tool_call) => {
            assert_eq!(tool_call.invocation.name, "bash");
            assert_eq!(tool_call.invocation.input["cmd"], "ls");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
    assert_eq!(response.text, "Hello");
    assert_eq!(response.model, ModelId::new(MODEL_SONNET_4_6));
    assert_eq!(response.usage.total_input_tokens(), 15);
    assert_eq!(response.usage.input_tokens_cache_read, 3);
    assert_eq!(response.usage.output_tokens, 5);
    assert_eq!(response.usage.input_tokens_uncached, 12);
    assert_eq!(response.usage.input_tokens_cache_write, 0);
    assert_eq!(response.usage.input_tokens_cache_read, 3);
    assert!(matches!(response.stop_reason, StopReason::ToolUse));
}

#[test]
fn supported_models_return_expected_capabilities() {
    let haiku_caps = capabilities_for_model(&canonical_model_id(MODEL_HAIKU_4_5).unwrap()).unwrap();
    let opus_caps = capabilities_for_model(&canonical_model_id(MODEL_OPUS_4_6).unwrap()).unwrap();
    let sonnet_caps =
        capabilities_for_model(&canonical_model_id(MODEL_SONNET_4_6).unwrap()).unwrap();

    assert_eq!(haiku_caps.context_window, 200_000);
    assert_eq!(opus_caps.context_window, 1_000_000);
    assert_eq!(sonnet_caps.context_window, 1_000_000);
    assert_eq!(haiku_caps.max_output, 64_000);
    assert_eq!(opus_caps.max_output, 128_000);
    assert_eq!(sonnet_caps.max_output, 128_000);
    assert!((haiku_caps.pricing.input_per_mtok - 1.0_f64).abs() < f64::EPSILON);
    assert!((opus_caps.pricing.input_per_mtok - 5.0_f64).abs() < f64::EPSILON);
    assert!((sonnet_caps.pricing.input_per_mtok - 3.0_f64).abs() < f64::EPSILON);
    assert_eq!(haiku_caps.model_id, ModelId::new(MODEL_HAIKU_4_5));
    assert_eq!(opus_caps.model_id, ModelId::new(MODEL_OPUS_4_6));
    assert_eq!(sonnet_caps.model_id, ModelId::new(MODEL_SONNET_4_6));
}

#[test]
fn model_ids_resolve_without_aliasing() {
    assert_eq!(
        canonical_model_id(MODEL_HAIKU_4_5).unwrap(),
        MODEL_HAIKU_4_5
    );
    assert_eq!(canonical_model_id(MODEL_OPUS_4_6).unwrap(), MODEL_OPUS_4_6);
    assert_eq!(
        canonical_model_id(MODEL_SONNET_4_6).unwrap(),
        MODEL_SONNET_4_6
    );
}

#[test]
fn provider_accepts_documented_default_models() {
    let provider = AnthropicProvider::new("test-key", MODEL_HAIKU_4_5).unwrap();
    assert_eq!(
        provider.capabilities().model_id,
        ModelId::new(MODEL_HAIKU_4_5)
    );

    let provider = AnthropicProvider::new("test-key", MODEL_SONNET_4_6).unwrap();
    assert_eq!(
        provider.capabilities().model_id,
        ModelId::new(MODEL_SONNET_4_6)
    );
}

#[test]
fn default_chat_provider_uses_a_bounded_concurrency_gate() {
    // Pins: a chat provider built from defaults gates in-flight requests with a
    // finite bound rather than queueing unbounded, so one process cannot open an
    // unlimited number of concurrent connections to the provider.
    let provider = AnthropicProvider::new("test-key", MODEL_HAIKU_4_5).unwrap();
    assert!(
        provider.limiter.is_bounded(),
        "chat concurrency must default to a bounded in-flight gate"
    );
}
