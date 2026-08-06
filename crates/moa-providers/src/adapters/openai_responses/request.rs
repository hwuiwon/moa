//! OpenAI Responses request mapping.

use super::streaming::{openai_native_tools, parse_reasoning_effort};
use super::tools::{
    metadata_as_strings, openai_tool_from_schema, openai_tool_result_output, responses_role,
    supports_reasoning,
};
use super::*;

const PREVIOUS_RESPONSE_ID_METADATA_KEY: &str = "_moa.openai.previous_response_id";
const TOOL_CHOICE_METADATA_KEY: &str = "_moa.openai.tool_choice";
const REASONING_EFFORT_METADATA_KEY: &str = "_moa.openai.reasoning_effort";

/// Builds one stateless Responses API request from MOA completion inputs.
pub(crate) fn build_responses_request<R: CompletionRequestView + ?Sized>(
    request: &R,
    default_model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<CreateResponse> {
    let mut instructions = Vec::new();
    let mut input_items = Vec::new();
    let mut in_leading_system_prefix = true;

    for message in request.messages() {
        if in_leading_system_prefix && message.role == MessageRole::System {
            instructions.push(message.content.clone());
            continue;
        }
        in_leading_system_prefix = false;

        if let Some(invocation) = message.tool_invocation.as_ref() {
            input_items.push(InputItem::Item(Item::FunctionCall(FunctionToolCall {
                arguments: serde_json::to_string(&invocation.input).map_err(MoaError::from)?,
                call_id: invocation
                    .id
                    .clone()
                    .unwrap_or_else(|| "unknown_tool_call".to_string()),
                namespace: None,
                name: invocation.name.clone(),
                id: invocation.id.clone(),
                status: None,
            })));
            continue;
        }

        if let Some(call_id) = message.tool_use_id.as_ref() {
            input_items.push(InputItem::Item(Item::FunctionCallOutput(
                FunctionCallOutputItemParam {
                    call_id: call_id.clone(),
                    output: openai_tool_result_output(message),
                    id: None,
                    status: None,
                },
            )));
            continue;
        }

        input_items.push(InputItem::EasyMessage(EasyInputMessage {
            r#type: Default::default(),
            role: if message.role == MessageRole::System {
                OpenAiRole::User
            } else {
                responses_role(message)
            },
            content: EasyInputContent::Text(message.content.clone()),
            phase: None,
        }));
    }

    if input_items.is_empty() {
        return Err(MoaError::ValidationError(
            "Responses requests require at least one non-system message".to_string(),
        ));
    }

    let mut tools = request
        .tools()
        .iter()
        .map(openai_tool_from_schema)
        .collect::<Result<Vec<_>>>()?;
    if request.response_format().is_none() {
        tools.extend(openai_native_tools(native_tools)?);
    }
    let has_tools = !tools.is_empty();
    let tools = if tools.is_empty() { None } else { Some(tools) };
    let uses_reasoning_controls = supports_reasoning(default_model);
    let reasoning_effort = metadata_string(request, REASONING_EFFORT_METADATA_KEY)
        .unwrap_or_else(|| default_reasoning_effort.to_string());
    let reasoning = if uses_reasoning_controls {
        Some(Reasoning {
            effort: Some(parse_reasoning_effort(&reasoning_effort)?),
            summary: None,
        })
    } else {
        None
    };

    Ok(CreateResponse {
        input: InputParam::Items(input_items),
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        model: Some(default_model.to_string()),
        prompt_cache_key: prompt_cache_key(request, default_model),
        prompt_cache_retention: Some(PromptCacheRetention::InMemory),
        tools,
        tool_choice: Some(tool_choice_param(request, has_tools)?),
        parallel_tool_calls: has_tools.then_some(true),
        max_output_tokens: request.max_output_tokens().map(|value| value as u32),
        metadata: metadata_as_strings(request.metadata()),
        reasoning,
        stream: Some(true),
        store: Some(false),
        previous_response_id: metadata_string(request, PREVIOUS_RESPONSE_ID_METADATA_KEY),
        text: request.response_format().map(openai_response_text_param),
        temperature: if uses_reasoning_controls {
            None
        } else {
            request.temperature()
        },
        ..CreateResponse::default()
    })
}

fn openai_response_text_param(format: &JsonResponseFormat) -> ResponseTextParam {
    let schema = if format.strict {
        compile_for_openai_strict(&format.schema)
    } else {
        format.schema.clone()
    };
    ResponseTextParam {
        format: TextResponseFormatConfiguration::JsonSchema(ResponseFormatJsonSchema {
            description: format.description.clone(),
            name: format.name.clone(),
            schema: Some(schema),
            strict: Some(format.strict),
        }),
        verbosity: None,
    }
}

fn prompt_cache_key<R: CompletionRequestView + ?Sized>(request: &R, model: &str) -> Option<String> {
    let prefix_fingerprint = stable_prefix_fingerprint(request);
    if prefix_fingerprint == 0 {
        return None;
    }

    Some(format!("moa:{model}:{prefix_fingerprint:016x}"))
}

fn metadata_string<R: CompletionRequestView + ?Sized>(request: &R, key: &str) -> Option<String> {
    request
        .metadata()
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn tool_choice_param<R: CompletionRequestView + ?Sized>(
    request: &R,
    has_tools: bool,
) -> Result<ToolChoiceParam> {
    let Some(choice) = metadata_string(request, TOOL_CHOICE_METADATA_KEY) else {
        return Ok(ToolChoiceParam::Mode(if has_tools {
            ToolChoiceOptions::Auto
        } else {
            ToolChoiceOptions::None
        }));
    };

    match choice.to_ascii_lowercase().as_str() {
        "none" => Ok(ToolChoiceParam::Mode(ToolChoiceOptions::None)),
        "auto" => Ok(ToolChoiceParam::Mode(ToolChoiceOptions::Auto)),
        "required" => Ok(ToolChoiceParam::Mode(ToolChoiceOptions::Required)),
        other => Err(MoaError::ValidationError(format!(
            "unsupported OpenAI Responses tool_choice '{other}'"
        ))),
    }
}
