//! Anthropic Messages API request construction.

use std::collections::HashSet;

use super::tools::{
    anthropic_message, anthropic_text_replay_message, anthropic_tool_from_schema,
    anthropic_tool_result_block, anthropic_tool_use_block, provider_native_tool_json,
};
use super::*;

pub(super) fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    capabilities: &ModelCapabilities,
    web_search_enabled: bool,
) -> Result<Value> {
    let mut system_messages = Vec::new();
    let mut messages = Vec::new();
    let mut in_leading_system_prefix = true;

    // Frozen-history boundary from the context pipeline: source messages
    // before this index replay byte-identically on later turns, so the last
    // built message under the boundary gets a moving cache breakpoint.
    let stable_history_end = request
        .metadata
        .get(moa_core::types::completion::STABLE_HISTORY_END_METADATA_KEY)
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let mut frozen_history_position = None;

    let mut index = 0;
    while index < request.messages.len() {
        if frozen_history_position.is_none()
            && stable_history_end.is_some_and(|boundary| index >= boundary)
            && !messages.is_empty()
        {
            frozen_history_position = Some(messages.len() - 1);
        }
        let message = &request.messages[index];
        if in_leading_system_prefix && message.role == MessageRole::System {
            system_messages.push(anthropic_text_block(message.content.clone()));
            index += 1;
            continue;
        }
        in_leading_system_prefix = false;

        if message.role == MessageRole::System {
            messages.push(anthropic_late_system_message(message));
            index += 1;
            continue;
        }

        if message.tool_invocation.is_some() {
            index += append_tool_exchange_or_text(&mut messages, &request.messages[index..])?;
            continue;
        }

        if message.role == MessageRole::Tool {
            messages.push(anthropic_text_replay_message(message));
            index += 1;
            continue;
        }

        messages.push(anthropic_message(message)?);
        index += 1;
    }
    if frozen_history_position.is_none()
        && stable_history_end.is_some_and(|boundary| boundary >= request.messages.len())
        && !messages.is_empty()
    {
        frozen_history_position = Some(messages.len() - 1);
    }

    if messages.is_empty() {
        return Err(MoaError::ValidationError(
            "Anthropic requests require at least one non-system message".to_string(),
        ));
    }

    let max_tokens = request
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        .min(capabilities.max_output);

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model.to_string()));
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), Value::Bool(true));

    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }

    let mut tools = request
        .tools
        .iter()
        .map(anthropic_tool_from_schema)
        .collect::<Vec<_>>();
    if web_search_enabled {
        tools.extend(
            capabilities
                .native_tools
                .iter()
                .map(provider_native_tool_json),
        );
    }
    let cache_estimate = CacheTokenEstimate::from_request(request);
    if cache_estimate.should_mark_stable_prefix() {
        mark_stable_prefix_cache_control(&mut system_messages, &mut tools);
    }
    if cache_estimate.should_enable_automatic_cache_control()
        && let Some(position) = frozen_history_position
        && let Some(message) = messages.get_mut(position)
    {
        // Second breakpoint at the frozen-history end: bytes up to here match
        // the previous turn's request (the pipeline keeps replayed history
        // append-only between checkpoints), so this span cache-reads across
        // turns even though everything after it changes per turn.
        mark_message_cache_control(message);
    }

    body.insert("messages".to_string(), Value::Array(messages));
    if !system_messages.is_empty() {
        body.insert("system".to_string(), Value::Array(system_messages));
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(response_format) = request.response_format.as_ref() {
        body.insert(
            "output_config".to_string(),
            anthropic_output_config(response_format),
        );
    }
    if cache_estimate.should_enable_automatic_cache_control() {
        body.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }

    Ok(Value::Object(body))
}

fn append_tool_exchange_or_text(
    messages: &mut Vec<Value>,
    window: &[ContextMessage],
) -> Result<usize> {
    let tool_call_count = window
        .iter()
        .take_while(|message| message.tool_invocation.is_some())
        .count();
    let tool_calls = &window[..tool_call_count];
    let expected_ids = tool_calls
        .iter()
        .filter_map(|message| message.tool_invocation.as_ref())
        .map(tool_invocation_id)
        .collect::<Vec<_>>();
    let unique_expected = expected_ids.iter().cloned().collect::<HashSet<_>>();

    if unique_expected.len() == expected_ids.len() {
        let mut matched_ids = HashSet::new();
        let mut result_count = 0usize;
        while let Some(message) = window.get(tool_call_count + result_count) {
            if message.role != MessageRole::Tool {
                break;
            }
            let tool_use_id = tool_result_id(message);
            if !unique_expected.contains(&tool_use_id) || !matched_ids.insert(tool_use_id) {
                break;
            }
            result_count += 1;
            if matched_ids.len() == unique_expected.len() {
                break;
            }
        }

        if matched_ids.len() == unique_expected.len() {
            let tool_use_blocks = tool_calls
                .iter()
                .filter_map(|message| message.tool_invocation.as_ref())
                .map(anthropic_tool_use_block)
                .collect::<Vec<_>>();
            let tool_result_blocks = window[tool_call_count..tool_call_count + result_count]
                .iter()
                .map(anthropic_tool_result_block)
                .collect::<Vec<_>>();
            messages.push(json!({
                "role": "assistant",
                "content": tool_use_blocks,
            }));
            messages.push(json!({
                "role": "user",
                "content": tool_result_blocks,
            }));
            return Ok(tool_call_count + result_count);
        }
    }

    for message in tool_calls {
        messages.push(anthropic_text_replay_message(message));
    }
    Ok(tool_call_count)
}

fn tool_invocation_id(invocation: &ToolInvocation) -> String {
    invocation
        .id
        .clone()
        .unwrap_or_else(|| "unknown_tool_use".to_string())
}

fn tool_result_id(message: &ContextMessage) -> String {
    message
        .tool_use_id
        .clone()
        .unwrap_or_else(|| "unknown_tool_use".to_string())
}

/// Builds an Anthropic request body for inspection tests without sending it.
pub fn debug_build_anthropic_request_body(
    request: &CompletionRequest,
    web_search_enabled: bool,
) -> Result<Value> {
    let requested_model = request
        .model
        .as_ref()
        .map(ModelId::as_str)
        .unwrap_or(MODEL_SONNET_4_6);
    let resolved_model = canonical_model_id(requested_model)?;
    let capabilities = capabilities_for_model(&resolved_model)?;
    build_request_body(request, &resolved_model, &capabilities, web_search_enabled)
}

fn anthropic_text_block(text: impl Into<String>) -> Value {
    json!({
        "type": "text",
        "text": text.into(),
    })
}

fn anthropic_late_system_message(message: &ContextMessage) -> Value {
    json!({
        "role": "user",
        "content": message.content,
    })
}

fn anthropic_output_config(format: &JsonResponseFormat) -> Value {
    json!({
        "format": {
            "type": "json_schema",
            "schema": format.schema,
        }
    })
}

/// Cached token estimate for the Anthropic request's cacheable regions.
///
/// The tool schemas and message contents are only serialized/estimated once here
/// and shared between the automatic and stable-prefix cache-control decisions,
/// avoiding re-serializing every tool and message up to twice per request.
struct CacheTokenEstimate {
    tool_tokens: usize,
    system_prefix_tokens: usize,
    all_message_tokens: usize,
}

impl CacheTokenEstimate {
    fn from_request(request: &CompletionRequest) -> Self {
        let tool_tokens = request
            .tools
            .iter()
            .map(|tool| estimate_text_tokens(&tool.to_string()))
            .sum::<usize>();

        let mut system_prefix_tokens = 0;
        let mut all_message_tokens = 0;
        let mut in_leading_system_prefix = true;
        for message in &request.messages {
            let tokens = estimate_text_tokens(&message.content);
            all_message_tokens += tokens;
            if in_leading_system_prefix && message.role == MessageRole::System {
                system_prefix_tokens += tokens;
            } else {
                in_leading_system_prefix = false;
            }
        }

        Self {
            tool_tokens,
            system_prefix_tokens,
            all_message_tokens,
        }
    }

    fn should_enable_automatic_cache_control(&self) -> bool {
        self.tool_tokens + self.all_message_tokens >= MIN_CACHEABLE_TOKENS
    }

    fn should_mark_stable_prefix(&self) -> bool {
        self.tool_tokens + self.system_prefix_tokens >= MIN_CACHEABLE_TOKENS
    }
}

/// Attaches an ephemeral cache marker to the last content block of one built
/// message, normalizing string content into a block array when needed.
fn mark_message_cache_control(message: &mut Value) {
    let Some(content) = message.get_mut("content") else {
        return;
    };
    match content {
        Value::String(text) => {
            if text.is_empty() {
                return;
            }
            let text = std::mem::take(text);
            *content = json!([{
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" },
            }]);
        }
        Value::Array(blocks) => {
            if let Some(last) = blocks.last_mut().and_then(Value::as_object_mut) {
                last.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
            }
        }
        _ => {}
    }
}

fn mark_stable_prefix_cache_control(system_messages: &mut [Value], tools: &mut [Value]) {
    if let Some(last_system) = system_messages.last_mut()
        && let Some(object) = last_system.as_object_mut()
    {
        object.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
        return;
    }

    if let Some(last_tool) = tools.last_mut()
        && let Some(object) = last_tool.as_object_mut()
    {
        object.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
    }
}
