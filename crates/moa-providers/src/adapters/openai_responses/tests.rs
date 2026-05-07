//! OpenAI Responses adapter unit tests.

use std::collections::HashMap;

use async_openai::error::OpenAIError;
use async_openai::types::responses::{
    PromptCacheRetention, ResponseUsage, TextResponseFormatConfiguration,
};
use moa_core::{CacheBreakpoint, CacheTtl, CompletionRequest, ContextMessage, JsonResponseFormat};
use serde_json::json;

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
        cache_breakpoints: vec![1],
        cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
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
        cache_breakpoints: Vec::new(),
        cache_controls: Vec::new(),
        metadata: HashMap::new(),
    };

    let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
        .expect("request should build");

    assert_eq!(built.temperature, None);
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
fn prompt_cache_key_ignores_dynamic_tail_messages() {
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
        cache_breakpoints: vec![1],
        cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
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
