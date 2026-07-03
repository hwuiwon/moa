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
    let mut in_leading_system_prefix = true;

    for message in &request.messages {
        if in_leading_system_prefix && message.role == MessageRole::System {
            system_messages.push(anthropic_text_block(message.content.clone()));
            continue;
        }
        in_leading_system_prefix = false;

        if message.role == MessageRole::System {
            messages.push(anthropic_late_system_message(message));
            continue;
        }

        messages.push(anthropic_message(message)?);
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
