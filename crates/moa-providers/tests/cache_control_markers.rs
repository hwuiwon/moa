//! Coverage for Anthropic prompt-cache marker placement.

use std::collections::BTreeMap;

use moa_core::{CacheBreakpoint, CacheTtl, CompletionRequest, ContextMessage, ModelId};
use moa_providers::debug_build_anthropic_request_body;
use serde_json::{Value, json};

const MODEL: &str = "claude-sonnet-4-6";
const LATEST_USER_TURN_A: &str = "Summarize only the new deployment risk.";
const LATEST_USER_TURN_B: &str = "Summarize only the new rollback risk.";

#[test]
fn anthropic_request_with_4_segment_pipeline_places_cache_markers_at_each_boundary() {
    let request = four_segment_request(CacheTtl::FiveMinutes, LATEST_USER_TURN_A, true);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 4);
    assert_four_segment_marker_layout(&body, "5m", LATEST_USER_TURN_A, true);
}

#[test]
fn anthropic_request_with_long_messages_keeps_cache_markers_at_segment_boundaries_not_message_boundaries()
 {
    let request = long_messages_request(LATEST_USER_TURN_A);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 4);
    assert_long_messages_marker_layout(&body);
}

#[test]
fn anthropic_request_with_no_tools_omits_tools_segment_marker() {
    let request = four_segment_request(CacheTtl::FiveMinutes, LATEST_USER_TURN_A, false);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 3);
    assert_four_segment_marker_layout(&body, "5m", LATEST_USER_TURN_A, false);
}

#[test]
fn anthropic_request_with_explicit_1h_ttl_includes_ttl_field_on_each_marker() {
    let request = four_segment_request(CacheTtl::OneHour, LATEST_USER_TURN_A, true);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 4);
    assert_cache_ttls(&body, "1h");
    assert_four_segment_marker_layout(&body, "1h", LATEST_USER_TURN_A, true);
}

#[test]
fn anthropic_request_byte_layout_is_identical_across_two_consecutive_turn_compilations() {
    let first = anthropic_body(&four_segment_request(
        CacheTtl::OneHour,
        LATEST_USER_TURN_A,
        true,
    ));
    let second = anthropic_body(&four_segment_request(
        CacheTtl::OneHour,
        LATEST_USER_TURN_B,
        true,
    ));
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
    let first = anthropic_body(&four_segment_request(
        CacheTtl::OneHour,
        LATEST_USER_TURN_A,
        true,
    ));
    let second = anthropic_body(&four_segment_request(
        CacheTtl::OneHour,
        LATEST_USER_TURN_B,
        true,
    ));
    let comparison = segment_comparison(&first, &second);

    assert_eq!(comparison.changed, vec!["messages"]);
    assert_eq!(
        comparison.unchanged,
        vec![
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

fn four_segment_request(
    ttl: CacheTtl,
    latest_user_turn: &str,
    include_tools: bool,
) -> CompletionRequest {
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
        cache_breakpoints: Vec::new(),
        cache_controls: vec![
            CacheBreakpoint::message(1, ttl),
            CacheBreakpoint::message(2, ttl),
            CacheBreakpoint::tools(ttl),
            CacheBreakpoint::message(4, ttl),
        ],
        metadata: Default::default(),
    }
}

fn long_messages_request(latest_user_turn: &str) -> CompletionRequest {
    let mut messages = vec![
        ContextMessage::system(stable_block("identity", 700)),
        ContextMessage::system(stable_block("instructions", 700)),
    ];
    for index in 0..20 {
        let content = format!(
            "stable history message {index}: {}",
            "history-token ".repeat(800)
        );
        if index % 2 == 0 {
            messages.push(ContextMessage::user(content));
        } else {
            messages.push(ContextMessage::assistant(content));
        }
    }
    messages.push(ContextMessage::user(latest_user_turn));

    CompletionRequest {
        model: Some(ModelId::new(MODEL)),
        messages,
        tools: vec![large_tool_schema()],
        max_output_tokens: Some(1024),
        temperature: Some(0.0),
        response_format: None,
        cache_breakpoints: Vec::new(),
        cache_controls: vec![
            CacheBreakpoint::message(1, CacheTtl::OneHour),
            CacheBreakpoint::message(2, CacheTtl::OneHour),
            CacheBreakpoint::tools(CacheTtl::OneHour),
            CacheBreakpoint::message(22, CacheTtl::OneHour),
        ],
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

fn assert_four_segment_marker_layout(
    body: &Value,
    expected_ttl: &str,
    expected_latest_turn: &str,
    include_tools: bool,
) {
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], json!(1024));
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["temperature"], json!(0.0));

    let system = array_field(body, "system");
    assert_eq!(system.len(), 2);
    assert_cache_control(&system[0], expected_ttl);
    assert_cache_control(&system[1], expected_ttl);

    let messages = array_field(body, "messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_no_cache_control(&messages[0]);
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], expected_latest_turn);
    assert_no_cache_control(&messages[2]);

    let assistant_blocks = messages[1]["content"]
        .as_array()
        .expect("cache-marked assistant message should use content blocks");
    assert_eq!(assistant_blocks.len(), 1);
    assert_cache_control(&assistant_blocks[0], expected_ttl);

    if include_tools {
        let tools = array_field(body, "tools");
        assert_eq!(tools.len(), 1);
        assert_cache_control(&tools[0], expected_ttl);
    } else {
        assert!(body.get("tools").is_none());
    }
}

fn assert_long_messages_marker_layout(body: &Value) {
    assert_eq!(body["model"], MODEL);

    let system = array_field(body, "system");
    assert_eq!(system.len(), 2);
    assert_cache_control(&system[0], "1h");
    assert_cache_control(&system[1], "1h");

    let messages = array_field(body, "messages");
    assert_eq!(messages.len(), 21);
    let marked_message_indexes = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (count_cache_control_markers(message) > 0).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(marked_message_indexes, vec![19]);

    let marker_blocks = messages[19]["content"]
        .as_array()
        .expect("cache-marked history message should use content blocks");
    assert_eq!(marker_blocks.len(), 1);
    assert_cache_control(&marker_blocks[0], "1h");
    assert_eq!(messages[20]["content"], LATEST_USER_TURN_A);
    assert_no_cache_control(&messages[20]);

    let tools = array_field(body, "tools");
    assert_eq!(tools.len(), 1);
    assert_cache_control(&tools[0], "1h");
}

fn array_field<'a>(value: &'a Value, field: &str) -> &'a Vec<Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("request body should contain array field {field}"))
}

fn assert_cache_control(value: &Value, expected_ttl: &str) {
    let cache_control = value
        .get("cache_control")
        .unwrap_or_else(|| panic!("value should carry a cache_control marker: {value}"));
    assert_eq!(cache_control["type"], "ephemeral");
    assert_eq!(cache_control["ttl"], expected_ttl);
}

fn assert_no_cache_control(value: &Value) {
    assert_eq!(count_cache_control_markers(value), 0);
}

fn assert_cache_ttls(value: &Value, expected_ttl: &str) {
    let mut ttls = Vec::new();
    collect_cache_ttls(value, &mut ttls);
    assert_eq!(ttls, vec![expected_ttl; 4]);
}

fn collect_cache_ttls<'a>(value: &'a Value, ttls: &mut Vec<&'a str>) {
    match value {
        Value::Object(object) => {
            if let Some(cache_control) = object.get("cache_control") {
                ttls.push(
                    cache_control
                        .get("ttl")
                        .and_then(Value::as_str)
                        .expect("cache_control marker should include ttl"),
                );
            }
            for value in object.values() {
                collect_cache_ttls(value, ttls);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_cache_ttls(value, ttls);
            }
        }
        _ => {}
    }
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
