//! Anthropic tool and message conversion helpers.

use super::*;

#[derive(Debug, Deserialize)]
pub(super) struct WebSearchResultContent {
    #[serde(default)]
    pub(super) title: String,
    #[serde(default)]
    pub(super) url: String,
}

pub(super) fn native_web_search_tools() -> Vec<ProviderNativeTool> {
    vec![ProviderNativeTool {
        tool_type: "web_search_20250305".to_string(),
        name: "web_search".to_string(),
        config: None,
    }]
}

pub(super) fn provider_native_tool_json(tool: &ProviderNativeTool) -> Value {
    let mut value = Map::new();
    value.insert("type".to_string(), Value::String(tool.tool_type.clone()));
    if !tool.name.is_empty() {
        value.insert("name".to_string(), Value::String(tool.name.clone()));
    }
    if let Some(config) = tool.config.as_ref()
        && let Some(object) = config.as_object()
    {
        for (key, entry) in object {
            value.insert(key.clone(), entry.clone());
        }
    }
    Value::Object(value)
}

pub(super) fn summarize_anthropic_server_tool_use(name: &str, partial_json: &str) -> String {
    if name == "web_search"
        && let Ok(value) = serde_json::from_str::<Value>(partial_json)
        && let Some(query) = value.get("query").and_then(Value::as_str)
    {
        return format!("Searching the web for: {query}");
    }

    format!("Running provider tool: {name}")
}

pub(super) fn summarize_anthropic_search_results(content: &[WebSearchResultContent]) -> String {
    if content.is_empty() {
        return "Web search completed.".to_string();
    }

    let first = &content[0];
    if !first.title.is_empty() {
        return format!(
            "Web search returned {} result(s). Top result: {}",
            content.len(),
            first.title
        );
    }
    if !first.url.is_empty() {
        return format!(
            "Web search returned {} result(s). Top result: {}",
            content.len(),
            first.url
        );
    }

    format!("Web search returned {} result(s).", content.len())
}

pub(super) fn anthropic_message(message: &ContextMessage) -> Result<Value> {
    if let Some(invocation) = message.tool_invocation.as_ref() {
        return Ok(json!({
            "role": "assistant",
            "content": [anthropic_tool_use_block(invocation)]
        }));
    }

    if message.role == MessageRole::Tool {
        return Ok(json!({
            "role": "user",
            "content": [anthropic_tool_result_block(message)]
        }));
    }

    let role = match message.role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => {
            return Err(MoaError::ProviderError(
                "unexpected System message in anthropic_message; should be filtered upstream"
                    .to_string(),
            ));
        }
        MessageRole::Tool => {
            return Err(MoaError::ProviderError(
                "unexpected Tool message in anthropic_message; should be filtered upstream"
                    .to_string(),
            ));
        }
    };

    Ok(json!({
        "role": role,
        "content": message.content,
    }))
}

pub(super) fn anthropic_text_replay_message(message: &ContextMessage) -> Value {
    let role = match message.role {
        MessageRole::Assistant => "assistant",
        MessageRole::User | MessageRole::System | MessageRole::Tool => "user",
    };
    json!({
        "role": role,
        "content": message.content,
    })
}

pub(super) fn anthropic_tool_use_block(invocation: &ToolInvocation) -> Value {
    json!({
        "type": "tool_use",
        "id": invocation
            .id
            .clone()
            .unwrap_or_else(|| "unknown_tool_use".to_string()),
        "name": invocation.name,
        "input": invocation.input,
    })
}

pub(super) fn anthropic_tool_result_block(message: &ContextMessage) -> Value {
    let content = if let Some(blocks) = &message.content_blocks {
        anthropic_content_blocks(blocks)
    } else {
        json!(message.content)
    };

    json!({
        "type": "tool_result",
        "tool_use_id": message
            .tool_use_id
            .clone()
            .unwrap_or_else(|| "unknown_tool_use".to_string()),
        "content": content,
    })
}

pub(super) fn anthropic_content_blocks(blocks: &[ToolContent]) -> Value {
    let mut rendered = Vec::with_capacity(blocks.len() + 2);
    rendered.push(json!({
        "type": "text",
        "text": "<untrusted_tool_output>",
    }));

    for block in blocks {
        match block {
            ToolContent::Text { text } => {
                rendered.push(json!({
                    "type": "text",
                    "text": text,
                }));
            }
            ToolContent::Json { data } => {
                rendered.push(json!({
                    "type": "text",
                    "text": data.to_string(),
                }));
            }
        }
    }

    rendered.push(json!({
        "type": "text",
        "text": "</untrusted_tool_output>",
    }));

    Value::Array(rendered)
}

pub(super) fn anthropic_tool_from_schema(schema: &Value) -> Value {
    if let Some(function) = schema.get("function") {
        return json!({
            "name": function.get("name").cloned().unwrap_or(Value::Null),
            "description": function.get("description").cloned().unwrap_or(Value::Null),
            "input_schema": function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        });
    }

    json!({
        "name": schema.get("name").cloned().unwrap_or(Value::Null),
        "description": schema.get("description").cloned().unwrap_or(Value::Null),
        "input_schema": schema
            .get("parameters")
            .or_else(|| schema.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}
