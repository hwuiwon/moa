//! Snapshot coverage for Anthropic prompt-cache marker placement.

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
    // Snapshot review: cache_control positions, system/tools/messages ordering, TTL spelling, dynamic tail.
    snapshot_json(
        "cache_control_markers__anthropic_request_with_4_segment_pipeline_places_cache_markers_at_each_boundary",
        &body,
    );
}

#[test]
fn anthropic_request_with_long_messages_keeps_cache_markers_at_segment_boundaries_not_message_boundaries()
 {
    let request = long_messages_request(LATEST_USER_TURN_A);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 4);
    // Snapshot review: exactly four cache_control blocks, marker on final stable history message, no per-message markers.
    snapshot_json(
        "cache_control_markers__anthropic_request_with_long_messages_keeps_cache_markers_at_segment_boundaries_not_message_boundaries",
        &body,
    );
}

#[test]
fn anthropic_request_with_no_tools_omits_tools_segment_marker() {
    let request = four_segment_request(CacheTtl::FiveMinutes, LATEST_USER_TURN_A, false);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 3);
    // Snapshot review: no tools field, system markers remain, message-segment marker remains.
    snapshot_json(
        "cache_control_markers__anthropic_request_with_no_tools_omits_tools_segment_marker",
        &body,
    );
}

#[test]
fn anthropic_request_with_explicit_1h_ttl_includes_ttl_field_on_each_marker() {
    let request = four_segment_request(CacheTtl::OneHour, LATEST_USER_TURN_A, true);
    let body = anthropic_body(&request);

    assert_eq!(count_cache_control_markers(&body), 4);
    assert_cache_ttls(&body, "1h");
    // Snapshot review: ttl fields on every cache_control block, especially the final tool and message markers.
    snapshot_json(
        "cache_control_markers__anthropic_request_with_explicit_1h_ttl_includes_ttl_field_on_each_marker",
        &body,
    );
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
    assert!(comparison.unchanged.contains(&"system"));
    assert!(comparison.unchanged.contains(&"tools"));
    // Snapshot review: changed segment list must stay messages-only; stable prefix segments stay unchanged.
    snapshot_json(
        "cache_control_markers__anthropic_request_byte_layout_changes_only_in_messages_segment_when_only_messages_change",
        &json!({
            "unchanged": comparison.unchanged,
            "changed": comparison.changed,
        }),
    );
}

fn anthropic_body(request: &CompletionRequest) -> Value {
    debug_build_anthropic_request_body(request, false).expect("Anthropic request body should build")
}

fn snapshot_json(name: &str, value: &Value) {
    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        insta::assert_json_snapshot!(name, value);
    });
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
