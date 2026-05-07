//! OpenAI Responses message, tool, and fallback-output helpers.

use super::*;

pub(super) fn responses_role(message: &ContextMessage) -> OpenAiRole {
    match message.role {
        MessageRole::System => OpenAiRole::System,
        MessageRole::User | MessageRole::Tool => OpenAiRole::User,
        MessageRole::Assistant => OpenAiRole::Assistant,
    }
}

pub(super) fn openai_tool_from_schema(schema: &Value) -> Result<Tool> {
    let compiled = compile_for_openai_strict(schema);

    if let Some(function) = compiled.get("function").and_then(Value::as_object) {
        return build_function_tool(
            function.get("name"),
            function.get("description"),
            function.get("parameters"),
            true,
        );
    }

    build_function_tool(
        compiled.get("name"),
        compiled.get("description"),
        compiled
            .get("parameters")
            .or_else(|| compiled.get("input_schema")),
        true,
    )
}

fn build_function_tool(
    name: Option<&Value>,
    description: Option<&Value>,
    parameters: Option<&Value>,
    strict: bool,
) -> Result<Tool> {
    let name = name
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::ValidationError("tool schema is missing a function name".to_string())
        })?
        .to_string();
    let description = description.and_then(Value::as_str).map(str::to_string);

    Ok(Tool::Function(FunctionTool {
        name,
        parameters: Some(
            parameters
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        ),
        strict: Some(strict),
        description,
        defer_loading: None,
    }))
}

pub(super) fn openai_tool_result_output(message: &ContextMessage) -> FunctionCallOutput {
    match message.content_blocks.as_ref() {
        Some(blocks) if !blocks.is_empty() => FunctionCallOutput::Content(
            blocks
                .iter()
                .map(|block| {
                    InputContent::InputText(InputTextContent {
                        text: match block {
                            ToolContent::Text { text } => text.clone(),
                            ToolContent::Json { data } => data.to_string(),
                        },
                    })
                })
                .collect(),
        ),
        _ => FunctionCallOutput::Text(message.content.clone()),
    }
}

pub(super) fn parse_tool_arguments(arguments: &str) -> Value {
    match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => Value::String(arguments.to_string()),
    }
}

pub(super) fn response_text_from_output(output: &[OutputItem]) -> String {
    let mut text = String::new();

    for item in output {
        if let OutputItem::Message(message) = item {
            for content in &message.content {
                match content {
                    OutputMessageContent::OutputText(part) => text.push_str(&part.text),
                    OutputMessageContent::Refusal(part) => text.push_str(&part.refusal),
                }
            }
        }
    }

    text
}

pub(super) fn response_content_from_output(
    output: &[OutputItem],
) -> Result<Vec<CompletionContent>> {
    let mut content = Vec::new();

    for item in output {
        match item {
            OutputItem::Message(message) => {
                for part in &message.content {
                    match part {
                        OutputMessageContent::OutputText(part) if !part.text.is_empty() => {
                            content.push(CompletionContent::Text(part.text.clone()));
                        }
                        OutputMessageContent::Refusal(part) if !part.refusal.is_empty() => {
                            content.push(CompletionContent::Text(part.refusal.clone()));
                        }
                        _ => {}
                    }
                }
            }
            OutputItem::FunctionCall(call) => {
                content.push(CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: parse_tool_arguments(&call.arguments),
                    },
                    provider_metadata: None,
                }));
            }
            OutputItem::WebSearchCall(call) => {
                content.push(CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: format!("Web search {}.", web_search_status(&call.status)),
                });
            }
            _ => {}
        }
    }

    Ok(content)
}

pub(super) fn response_stop_reason(response: &Response) -> StopReason {
    match response.status {
        OpenAiStatus::Cancelled => StopReason::Cancelled,
        OpenAiStatus::Incomplete => response
            .incomplete_details
            .as_ref()
            .map(|details| match details.reason.as_str() {
                "max_output_tokens" => StopReason::MaxTokens,
                other => StopReason::Other(other.to_string()),
            })
            .unwrap_or_else(|| StopReason::Other("incomplete".to_string())),
        OpenAiStatus::Failed => StopReason::Other("failed".to_string()),
        _ => {
            if response
                .output
                .iter()
                .any(|item| matches!(item, OutputItem::FunctionCall(_)))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
    }
}

fn web_search_status(status: &WebSearchToolCallStatus) -> &'static str {
    match status {
        WebSearchToolCallStatus::InProgress => "started",
        WebSearchToolCallStatus::Searching => "searching",
        WebSearchToolCallStatus::Completed => "completed",
        WebSearchToolCallStatus::Failed => "failed",
    }
}

pub(super) fn metadata_as_strings(
    metadata: &HashMap<String, Value>,
) -> Option<HashMap<String, String>> {
    if metadata.is_empty() {
        return None;
    }

    let filtered: HashMap<String, String> = metadata
        .iter()
        .filter_map(|(key, value)| {
            if key.starts_with("_moa.") {
                return None;
            }

            let value = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());

            if value.len() > OPENAI_METADATA_VALUE_LIMIT {
                tracing::debug!(
                    key,
                    value_len = value.len(),
                    "dropping oversized Responses metadata value"
                );
                return None;
            }

            Some((key.clone(), value))
        })
        .collect();

    (!filtered.is_empty()).then_some(filtered)
}

pub(super) fn supports_reasoning(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with('o')
}
