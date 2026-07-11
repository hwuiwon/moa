//! Coverage for provider-owned prompt cache behavior.

use std::collections::BTreeMap;

use moa_core::{
    types::completion::CompletionRequest, types::context::ContextMessage,
    types::identifiers::ModelId,
};
use moa_providers::debug_build_anthropic_request_body;
use serde_json::{Value, json};

const MODEL: &str = "claude-sonnet-4-6";
const LATEST_USER_TURN_A: &str = "Summarize only the new deployment risk.";
const LATEST_USER_TURN_B: &str = "Summarize only the new rollback risk.";

#[test]
fn anthropic_large_request_uses_top_level_and_stable_prefix_cache_control() {
    // Pins: Anthropic cache control is provider-owned with one stable-prefix marker.
    let body = anthropic_body(&four_segment_request(LATEST_USER_TURN_A, true));

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(nested_cache_control_markers(&body), 1);
    assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    assert!(!body["messages"].to_string().contains("cache_control"));
}

#[test]
fn anthropic_request_with_no_tools_keeps_stable_system_layout() {
    // Pins: removing tools does not change the stable system section ordering.
    let body = anthropic_body(&four_segment_request(LATEST_USER_TURN_A, false));

    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert!(body.get("tools").is_none());
    assert_four_segment_layout(&body, LATEST_USER_TURN_A, false);
}

#[test]
fn anthropic_request_byte_layout_is_identical_before_dynamic_user_turn() {
    // Pins: bytes before the active user turn stay identical across dynamic tail changes.
    let first = anthropic_body(&four_segment_request(LATEST_USER_TURN_A, true));
    let second = anthropic_body(&four_segment_request(LATEST_USER_TURN_B, true));
    let first_bytes = serde_json::to_vec(&first).expect("serialize first request body");
    let second_bytes = serde_json::to_vec(&second).expect("serialize second request body");
    let first_prefix_len = byte_position(&first_bytes, LATEST_USER_TURN_A.as_bytes());
    let second_prefix_len = byte_position(&second_bytes, LATEST_USER_TURN_B.as_bytes());

    assert_eq!(
        &first_bytes[..first_prefix_len],
        &second_bytes[..second_prefix_len],
        "Anthropic request bytes before the changing final user turn must stay identical"
    );
}

#[test]
fn anthropic_request_byte_layout_changes_only_in_messages_segment_when_only_messages_change() {
    // Pins: stable request fields remain byte-stable when only message tail content changes.
    let first = anthropic_body(&four_segment_request(LATEST_USER_TURN_A, true));
    let second = anthropic_body(&four_segment_request(LATEST_USER_TURN_B, true));
    let comparison = segment_comparison(&first, &second);

    assert_eq!(comparison.changed, vec!["messages"]);
    assert_eq!(
        comparison.unchanged,
        vec![
            "cache_control",
            "max_tokens",
            "model",
            "stream",
            "system",
            "temperature",
            "tools"
        ],
    );
}

fn anthropic_body(request: &CompletionRequest) -> Value {
    debug_build_anthropic_request_body(request, false).expect("Anthropic request body should build")
}

fn four_segment_request(latest_user_turn: &str, include_tools: bool) -> CompletionRequest {
    let tools = if include_tools {
        vec![large_tool_schema()]
    } else {
        Vec::new()
    };
    CompletionRequest {
        model: Some(ModelId::new(MODEL)),
        messages: vec![
            ContextMessage::system(stable_block("identity", 700)),
            ContextMessage::system(stable_block("instructions", 700)),
            ContextMessage::user(stable_block("prior user message", 700)),
            ContextMessage::assistant(stable_block("prior assistant message", 700)),
            ContextMessage::user(latest_user_turn),
        ],
        tools,
        max_output_tokens: Some(1024),
        temperature: Some(0.0),
        response_format: None,
        metadata: Default::default(),
    }
}

fn stable_block(label: &str, repeats: usize) -> String {
    format!("{label}: {}", "stable-prefix ".repeat(repeats))
}

fn large_tool_schema() -> Value {
    json!({
        "name": "file_search",
        "description": "Search indexed workspace files. ".repeat(700),
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query"
                }
            },
            "required": ["query"]
        }
    })
}

fn assert_four_segment_layout(body: &Value, expected_latest_turn: &str, include_tools: bool) {
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], json!(1024));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["temperature"], json!(0.0));

    let system = array_field(body, "system");
    assert_eq!(system.len(), 2);
    assert!(system[0].get("cache_control").is_none());
    assert_eq!(system[1]["cache_control"]["type"], "ephemeral");

    let messages = array_field(body, "messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], expected_latest_turn);

    if include_tools {
        let tools = array_field(body, "tools");
        assert_eq!(tools.len(), 1);
        assert!(tools[0].get("cache_control").is_none());
    } else {
        assert!(body.get("tools").is_none());
    }
}

fn nested_cache_control_markers(value: &Value) -> usize {
    match value {
        Value::Object(object) => object
            .iter()
            .filter(|(key, _)| key.as_str() != "cache_control")
            .map(|(_, value)| count_cache_control_markers(value))
            .sum(),
        Value::Array(values) => values.iter().map(count_cache_control_markers).sum(),
        _ => 0,
    }
}

fn count_cache_control_markers(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            object
                .keys()
                .filter(|key| key.as_str() == "cache_control")
                .count()
                + object
                    .values()
                    .map(count_cache_control_markers)
                    .sum::<usize>()
        }
        Value::Array(values) => values.iter().map(count_cache_control_markers).sum(),
        _ => 0,
    }
}

fn array_field<'a>(value: &'a Value, field: &str) -> &'a Vec<Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("request body should contain array field {field}"))
}

fn byte_position(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("dynamic user message should appear in serialized request")
}

#[derive(Debug)]
struct SegmentComparison<'a> {
    unchanged: Vec<&'a str>,
    changed: Vec<&'a str>,
}

fn segment_comparison<'a>(first: &Value, second: &Value) -> SegmentComparison<'a> {
    let mut hashes = BTreeMap::new();
    for segment in [
        "cache_control",
        "max_tokens",
        "model",
        "stream",
        "system",
        "temperature",
        "tools",
        "messages",
    ] {
        hashes.insert(
            segment,
            (field_hash(first, segment), field_hash(second, segment)),
        );
    }

    let mut unchanged = Vec::new();
    let mut changed = Vec::new();
    for (segment, (first_hash, second_hash)) in hashes {
        if first_hash == second_hash {
            unchanged.push(segment);
        } else {
            changed.push(segment);
        }
    }

    SegmentComparison { unchanged, changed }
}

fn field_hash(value: &Value, field: &str) -> u64 {
    let field_value = value
        .get(field)
        .unwrap_or_else(|| panic!("request body should contain {field}"));
    stable_hash(&serde_json::to_vec(field_value).expect("serialize request segment"))
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
