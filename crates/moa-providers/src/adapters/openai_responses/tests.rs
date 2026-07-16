//! OpenAI Responses adapter unit tests.

use std::collections::HashMap;

use async_openai::error::OpenAIError;
use async_openai::types::responses::{
    PromptCacheRetention, ResponseUsage, TextResponseFormatConfiguration,
};
use moa_core::{
    types::completion::CompletionRequest, types::completion::JsonResponseFormat,
    types::context::ContextMessage, types::model::ProviderNativeTool,
};
use serde_json::json;

use super::provider::debug_native_tools_for_request;
use super::{
    build_responses_request, is_ignorable_openai_stream_error, metadata_as_strings,
    token_usage_from_openai_usage,
};

#[test]
fn metadata_as_strings_drops_internal_moa_keys() {
    let metadata = HashMap::from([
        ("_moa.session_id".to_string(), json!("session-123")),
        ("visible".to_string(), json!("value")),
    ]);

    let filtered = metadata_as_strings(&metadata).expect("filtered metadata");

    assert_eq!(filtered.get("visible").map(String::as_str), Some("value"));
    assert!(!filtered.contains_key("_moa.session_id"));
}

#[test]
fn ignores_web_search_output_done_incomplete_status() {
    let decode_error = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
    let error = OpenAIError::JSONDeserialize(
            decode_error,
            "{\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"status\":\"incomplete\"}}".to_string(),
        );

    assert!(is_ignorable_openai_stream_error(&error));
}

#[test]
fn token_usage_from_openai_usage_splits_cached_prompt_tokens() {
    let usage: ResponseUsage = serde_json::from_value(json!({
        "input_tokens": 2048,
        "output_tokens": 512,
        "total_tokens": 2560,
        "input_tokens_details": {
            "cached_tokens": 1536
        },
        "output_tokens_details": {
            "reasoning_tokens": 0
        }
    }))
    .expect("usage fixture should deserialize");

    let token_usage = token_usage_from_openai_usage(&usage);
    assert_eq!(token_usage.input_tokens_uncached, 512);
    assert_eq!(token_usage.input_tokens_cache_write, 0);
    assert_eq!(token_usage.input_tokens_cache_read, 1536);
    assert_eq!(token_usage.output_tokens, 512);
}

#[test]
fn build_responses_request_sets_prompt_cache_key_and_retention() {
    // Pins: OpenAI cache keys are provider-derived from the stable prompt prefix.
    let request = CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system("Static instructions".to_string()),
            ContextMessage::user("Current task".to_string()),
        ],
        tools: vec![json!({
            "name": "echo",
            "description": "Echo tool",
            "input_schema": {
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }
        })],
        max_output_tokens: Some(128),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };

    let built =
        build_responses_request(&request, "gpt-5.4", "medium", &[]).expect("request should build");

    assert_eq!(
        built.prompt_cache_retention,
        Some(PromptCacheRetention::InMemory)
    );
    assert!(
        built
            .prompt_cache_key
            .as_deref()
            .is_some_and(|key| key.starts_with("moa:gpt-5.4:")),
        "expected a stable OpenAI prompt cache key"
    );
}

#[test]
fn build_responses_request_omits_temperature_for_reasoning_models() {
    let request = CompletionRequest {
        model: None,
        messages: vec![ContextMessage::user("Rewrite this query")],
        tools: Vec::new(),
        max_output_tokens: Some(128),
        temperature: Some(0.0),
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };

    let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
        .expect("request should build");

    assert_eq!(built.temperature, None);
}

#[test]
fn build_responses_request_allows_reasoning_effort_metadata_override() {
    let mut request = CompletionRequest::new("Rewrite this query");
    request
        .metadata
        .insert("_moa.openai.reasoning_effort".to_string(), json!("minimal"));

    let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
        .expect("request should build");

    assert_eq!(
        serde_json::to_value(built.reasoning.expect("reasoning config")).unwrap()["effort"],
        json!("minimal")
    );
}

#[test]
fn build_responses_request_sets_structured_output_schema() {
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

    let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
        .expect("request should build");
    let text = built.text.expect("structured output text config");
    let TextResponseFormatConfiguration::JsonSchema(schema) = text.format else {
        panic!("expected json_schema text format");
    };

    assert_eq!(schema.name, "test_payload");
    assert_eq!(schema.strict, Some(true));
    assert_eq!(
        schema
            .schema
            .and_then(|schema| schema.get("required").cloned()),
        Some(json!(["answer"]))
    );
}

#[test]
fn build_responses_request_compiles_strict_structured_output_schema() {
    // Pins: strict structured outputs use the same recursive OpenAI schema
    // normalization as tools while leaving the canonical domain schema untouched.
    let canonical_schema = json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "api_version": { "const": "moa.artifact/v1" },
            "kind": { "enum": ["experiment_plan"] },
            "metadata": {
                "type": "object",
                "additionalProperties": true,
                "properties": {
                    "name": { "type": "string" },
                    "description": { "type": "string" }
                },
                "required": ["name"]
            }
        },
        "required": ["api_version", "kind", "metadata"]
    });
    let original_schema = canonical_schema.clone();
    let mut request = CompletionRequest::new("Return an experiment plan.");
    request.response_format = Some(JsonResponseFormat::strict_json_schema(
        "experiment_plan",
        "Generated plan.",
        canonical_schema,
    ));

    let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
        .expect("strict structured request should build");
    let text = built
        .text
        .expect("strict request should include text format");
    let TextResponseFormatConfiguration::JsonSchema(format) = text.format else {
        panic!("expected json_schema text format");
    };
    let schema = format
        .schema
        .expect("strict format should include a schema");

    assert_eq!(schema["additionalProperties"], json!(false));
    assert_eq!(schema["properties"]["api_version"]["type"], json!("string"));
    assert_eq!(schema["properties"]["kind"]["type"], json!("string"));
    assert_eq!(
        schema["properties"]["metadata"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        schema["properties"]["metadata"]["required"],
        json!(["name", "description"])
    );
    assert_eq!(
        schema["properties"]["metadata"]["properties"]["description"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        request
            .response_format
            .as_ref()
            .expect("request keeps response format")
            .schema,
        original_schema
    );
}

#[test]
fn build_responses_request_omits_native_tools_for_structured_output() {
    // Pins: structured extraction calls must be direct model calls; provider-native
    // tools such as web search add latency and can be incompatible with minimal reasoning.
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
    let native_tools = vec![ProviderNativeTool {
        tool_type: "web_search".to_string(),
        name: "web_search".to_string(),
        config: None,
    }];

    let built = build_responses_request(&request, "gpt-5.4-nano", "medium", &native_tools)
        .expect("request should build");
    let serialized = serde_json::to_value(&built).expect("request should serialize");

    let tools_value = serialized.get("tools");
    assert!(
        tools_value.is_none() || tools_value.is_some_and(serde_json::Value::is_null),
        "structured calls should not carry native tools: {serialized}"
    );
    assert_eq!(serialized["tool_choice"], json!("none"));
    assert!(
        !serialized.to_string().contains("web_search"),
        "structured calls should not include web search: {serialized}"
    );
}

#[test]
fn native_web_search_policy_disables_openai_native_tool_per_request() {
    // Pins: request-scoped planning policy narrows globally enabled OpenAI web search.
    let mut request = CompletionRequest::new("Plan without web search.");
    assert!(!debug_native_tools_for_request(&request, true).is_empty());
    request.native_web_search = moa_core::types::completion::NativeWebSearchPolicy::Disabled;
    assert!(debug_native_tools_for_request(&request, true).is_empty());
}

#[test]
fn build_responses_request_keeps_late_system_messages_in_input_items() {
    // Pins: only leading system messages become OpenAI instructions.
    let request = CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system("Stable instructions"),
            ContextMessage::user("First task"),
            ContextMessage::system("Dynamic security reminder"),
            ContextMessage::user("Second task"),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(128),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };

    let built =
        build_responses_request(&request, "gpt-5.4", "medium", &[]).expect("request should build");
    let serialized = serde_json::to_value(&built).expect("request should serialize");

    assert_eq!(built.instructions.as_deref(), Some("Stable instructions"));
    assert_eq!(serialized["input"][0]["content"], "First task");
    assert_eq!(serialized["input"][1]["role"], "user");
    assert_eq!(
        serialized["input"][1]["content"],
        "Dynamic security reminder"
    );
    assert_eq!(serialized["input"][2]["content"], "Second task");
}

#[test]
fn prompt_cache_key_ignores_dynamic_tail_messages() {
    // Pins: OpenAI prompt_cache_key remains stable when only non-system tail messages change.
    let mut first = CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system("Static instructions".to_string()),
            ContextMessage::user("Tail one".to_string()),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(128),
        temperature: None,
        response_format: None,
        native_web_search: Default::default(),
        metadata: HashMap::new(),
    };
    let mut second = first.clone();
    first
        .messages
        .push(ContextMessage::assistant("Dynamic assistant A"));
    second
        .messages
        .push(ContextMessage::assistant("Dynamic assistant B"));

    let first_built =
        build_responses_request(&first, "gpt-5.4", "medium", &[]).expect("first request");
    let second_built =
        build_responses_request(&second, "gpt-5.4", "medium", &[]).expect("second request");

    assert_eq!(first_built.prompt_cache_key, second_built.prompt_cache_key);
}
