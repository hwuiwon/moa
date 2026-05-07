//! Anthropic Messages API request construction.

use super::tools::{anthropic_message, anthropic_tool_from_schema, provider_native_tool_json};
use super::*;

pub(super) fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    capabilities: &ModelCapabilities,
    web_search_enabled: bool,
) -> Result<Value> {
    let mut system_messages = Vec::new();
    let mut messages = Vec::new();
    let mut cache_targets = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        if message.role == MessageRole::System {
            let system_index = system_messages.len();
            system_messages.push(anthropic_text_block(message.content.clone()));
            cache_targets.push(CacheTarget::System(system_index));
            continue;
        }

        let message_index = messages.len();
        messages.push(anthropic_message(message)?);
        cache_targets.push(CacheTarget::Message(message_index));
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

    apply_cache_breakpoints(
        request,
        &cache_targets,
        &mut system_messages,
        &mut messages,
        &mut tools,
    );

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

    Ok(Value::Object(body))
}

#[cfg(feature = "test-util")]
pub(super) fn debug_build_anthropic_request_body(
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

fn anthropic_output_config(format: &JsonResponseFormat) -> Value {
    json!({
        "format": {
            "type": "json_schema",
            "schema": format.schema,
        }
    })
}

fn apply_cache_breakpoints(
    request: &CompletionRequest,
    cache_targets: &[CacheTarget],
    system_messages: &mut [Value],
    messages: &mut [Value],
    tools: &mut [Value],
) {
    for breakpoint in eligible_cache_breakpoints(request, MAX_CACHE_BREAKPOINTS) {
        match breakpoint.target {
            CacheBreakpointTarget::ToolDefinitions => {
                if let Some(last_tool) = tools.last_mut() {
                    annotate_cache_control(last_tool, breakpoint.ttl);
                }
            }
            CacheBreakpointTarget::MessageBoundary { index } => {
                let Some(target_index) = index.checked_sub(1) else {
                    continue;
                };
                let Some(target) = cache_targets.get(target_index).copied() else {
                    continue;
                };

                match target {
                    CacheTarget::System(index) => {
                        if let Some(block) = system_messages.get_mut(index) {
                            annotate_cache_control(block, breakpoint.ttl);
                        }
                    }
                    CacheTarget::Message(index) => {
                        if let Some(message) = messages.get_mut(index) {
                            annotate_message_cache_control(message, breakpoint.ttl);
                        }
                    }
                }
            }
        }
    }
}

fn eligible_cache_breakpoints(
    request: &CompletionRequest,
    max_breakpoints: usize,
) -> Vec<CacheBreakpoint> {
    let mut requested = requested_cache_breakpoints(request);
    if requested.is_empty() || max_breakpoints == 0 {
        return Vec::new();
    }

    requested.sort_by_key(cache_breakpoint_sort_key);
    requested.dedup();

    let mut eligible = Vec::new();
    let tool_tokens = request
        .tools
        .iter()
        .map(|tool| estimate_text_tokens(&tool.to_string()))
        .sum::<usize>();
    let mut prefix_tokens = tool_tokens;
    let mut next_breakpoint = 0usize;

    while let Some(breakpoint) = requested.get(next_breakpoint).cloned() {
        match breakpoint.target {
            CacheBreakpointTarget::ToolDefinitions => {
                if tool_tokens >= MIN_CACHEABLE_TOKENS {
                    eligible.push(breakpoint);
                }
                next_breakpoint += 1;
            }
            CacheBreakpointTarget::MessageBoundary { .. } => break,
        }
    }

    for (index, message) in request.messages.iter().enumerate() {
        prefix_tokens += estimate_text_tokens(&message.content);

        while let Some(breakpoint) = requested.get(next_breakpoint).cloned() {
            let CacheBreakpointTarget::MessageBoundary {
                index: breakpoint_index,
            } = breakpoint.target
            else {
                break;
            };
            if breakpoint_index != index + 1 {
                break;
            }
            if prefix_tokens >= MIN_CACHEABLE_TOKENS {
                eligible.push(breakpoint);
            }
            next_breakpoint += 1;
        }
    }

    eligible
        .into_iter()
        .rev()
        .take(max_breakpoints)
        .rev()
        .collect()
}

fn requested_cache_breakpoints(request: &CompletionRequest) -> Vec<CacheBreakpoint> {
    if !request.cache_controls.is_empty() {
        return request.cache_controls.clone();
    }

    request
        .cache_breakpoints
        .iter()
        .copied()
        .filter(|breakpoint| *breakpoint > 0)
        .map(|index| CacheBreakpoint::message(index, CacheTtl::OneHour))
        .collect()
}

fn cache_breakpoint_sort_key(breakpoint: &CacheBreakpoint) -> (usize, usize) {
    match breakpoint.target {
        CacheBreakpointTarget::ToolDefinitions => (0, 0),
        CacheBreakpointTarget::MessageBoundary { index } => (1, index),
    }
}

fn annotate_cache_control(value: &mut Value, ttl: CacheTtl) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "cache_control".to_string(),
            json!({
                "type": "ephemeral",
                "ttl": ttl.as_anthropic_ttl(),
            }),
        );
    }
}

fn annotate_message_cache_control(message: &mut Value, ttl: CacheTtl) {
    let Some(content) = message.get_mut("content") else {
        return;
    };

    match content {
        Value::String(text) => {
            let text = std::mem::take(text);
            *content = Value::Array(vec![anthropic_text_block(text)]);
            if let Some(blocks) = content.as_array_mut()
                && let Some(last_block) = blocks.last_mut()
            {
                annotate_cache_control(last_block, ttl);
            }
        }
        Value::Array(blocks) => {
            if let Some(last_block) = blocks.last_mut() {
                annotate_cache_control(last_block, ttl);
            }
        }
        _ => {}
    }
}
