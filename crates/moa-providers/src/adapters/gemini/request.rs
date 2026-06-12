//! Gemini request construction.

use std::collections::HashMap;

use super::tools::{
    content_message, flush_pending_parts, flush_tool_responses, function_call_part,
    function_response_part, gemini_function_declaration, is_standard_user_message, text_part,
    thinking_config_for_request,
};
use super::*;

const SAFETY_THRESHOLD_METADATA_KEY: &str = "_moa.gemini.safety_threshold";
const DEFAULT_SAFETY_THRESHOLD: &str = "BLOCK_MEDIUM_AND_ABOVE";
const SAFETY_CATEGORIES: [&str; 4] = [
    "HARM_CATEGORY_HARASSMENT",
    "HARM_CATEGORY_HATE_SPEECH",
    "HARM_CATEGORY_SEXUALLY_EXPLICIT",
    "HARM_CATEGORY_DANGEROUS_CONTENT",
];
const SAFETY_THRESHOLDS: [&str; 5] = [
    "HARM_BLOCK_THRESHOLD_UNSPECIFIED",
    "BLOCK_LOW_AND_ABOVE",
    "BLOCK_MEDIUM_AND_ABOVE",
    "BLOCK_ONLY_HIGH",
    "BLOCK_NONE",
];

struct GeminiRequestParts {
    system_instruction: Option<Value>,
    contents: Vec<Value>,
    tools: Vec<Value>,
    generation_config: Option<Value>,
}

struct GeminiRequestBuildOptions<'a> {
    model: &'a str,
    max_output_tokens: Option<usize>,
    temperature: Option<f32>,
    response_format: Option<&'a JsonResponseFormat>,
    default_reasoning_effort: &'a str,
    native_tools: &'a [ProviderNativeTool],
    include_tools: bool,
}

pub(super) fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<Value> {
    let parts = build_request_parts(
        &request.messages,
        &request.tools,
        GeminiRequestBuildOptions {
            model,
            max_output_tokens: request.max_output_tokens,
            temperature: request.temperature,
            response_format: request.response_format.as_ref(),
            default_reasoning_effort,
            native_tools,
            include_tools: true,
        },
    )?;
    build_request_body_from_parts(parts, build_safety_settings(&request.metadata)?)
}

fn build_request_parts(
    messages: &[ContextMessage],
    tools: &[Value],
    options: GeminiRequestBuildOptions<'_>,
) -> Result<GeminiRequestParts> {
    let (system_instruction, contents) = build_contents_from_messages(messages);
    let generation_config = build_generation_config(
        options.model,
        options.max_output_tokens,
        options.temperature,
        options.response_format,
        options.default_reasoning_effort,
    )?;
    let tools = if options.include_tools {
        build_tools(tools, options.native_tools)?
    } else {
        Vec::new()
    };

    Ok(GeminiRequestParts {
        system_instruction,
        contents,
        tools,
        generation_config,
    })
}

fn build_request_body_from_parts(
    parts: GeminiRequestParts,
    safety_settings: Vec<Value>,
) -> Result<Value> {
    if parts.contents.is_empty() {
        return Err(MoaError::ValidationError(
            "Gemini requests require at least one non-system message".to_string(),
        ));
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(parts.contents));
    if let Some(system_instruction) = parts.system_instruction {
        body.insert("systemInstruction".to_string(), system_instruction);
    }
    if !parts.tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(parts.tools));
    }
    if let Some(generation_config) = parts.generation_config {
        body.insert("generationConfig".to_string(), generation_config);
    }
    if !safety_settings.is_empty() {
        body.insert("safetySettings".to_string(), Value::Array(safety_settings));
    }

    Ok(Value::Object(body))
}

fn build_safety_settings(metadata: &HashMap<String, Value>) -> Result<Vec<Value>> {
    let threshold = metadata
        .get(SAFETY_THRESHOLD_METADATA_KEY)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SAFETY_THRESHOLD);

    if !SAFETY_THRESHOLDS.contains(&threshold) {
        return Err(MoaError::ValidationError(format!(
            "unsupported Gemini safety threshold '{threshold}'"
        )));
    }

    Ok(SAFETY_CATEGORIES
        .iter()
        .map(|category| {
            json!({
                "category": category,
                "threshold": threshold,
            })
        })
        .collect())
}

fn build_contents_from_messages(messages: &[ContextMessage]) -> (Option<Value>, Vec<Value>) {
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut model_parts = Vec::new();
    let mut tool_response_parts = Vec::new();
    let mut tool_names_by_id = HashMap::new();
    let mut in_leading_system_prefix = true;

    for message in messages {
        if in_leading_system_prefix && message.role == MessageRole::System {
            flush_pending_parts(&mut contents, &mut model_parts, &mut tool_response_parts);
            if !message.content.is_empty() || message.thought_signature.is_some() {
                system_parts.push(text_part(
                    message.content.as_str(),
                    message.thought_signature.as_deref(),
                ));
            }
            continue;
        }
        in_leading_system_prefix = false;

        if is_standard_user_message(message) || message.role == MessageRole::System {
            flush_pending_parts(&mut contents, &mut model_parts, &mut tool_response_parts);
            contents.push(content_message(
                "user",
                vec![text_part(
                    message.content.as_str(),
                    message.thought_signature.as_deref(),
                )],
            ));
            continue;
        }

        if let Some(invocation) = message.tool_invocation.as_ref() {
            if !tool_response_parts.is_empty() {
                flush_tool_responses(&mut contents, &mut tool_response_parts);
            }
            model_parts.push(function_call_part(
                invocation,
                message.thought_signature.as_deref(),
            ));
            if let Some(id) = invocation.id.as_ref() {
                tool_names_by_id.insert(id.clone(), invocation.name.clone());
            }
            continue;
        }

        if message.role == MessageRole::Assistant {
            if !tool_response_parts.is_empty() {
                flush_tool_responses(&mut contents, &mut tool_response_parts);
            }
            if !message.content.is_empty() || message.thought_signature.is_some() {
                model_parts.push(text_part(
                    message.content.as_str(),
                    message.thought_signature.as_deref(),
                ));
            }
            continue;
        }

        if message.role == MessageRole::Tool {
            if let Some(call_id) = message.tool_use_id.as_ref()
                && let Some(name) = tool_names_by_id.get(call_id).cloned()
            {
                tool_response_parts.push(function_response_part(&name, call_id, message));
            } else {
                tool_response_parts.push(text_part(message.content.as_str(), None));
            }
        }
    }

    flush_pending_parts(&mut contents, &mut model_parts, &mut tool_response_parts);

    let system_instruction = (!system_parts.is_empty()).then(|| json!({ "parts": system_parts }));
    (system_instruction, contents)
}

fn build_generation_config(
    model: &str,
    max_output_tokens: Option<usize>,
    temperature: Option<f32>,
    response_format: Option<&JsonResponseFormat>,
    default_reasoning_effort: &str,
) -> Result<Option<Value>> {
    let mut generation_config = Map::new();
    if let Some(max_output_tokens) = max_output_tokens {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
    }
    if let Some(temperature) = temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(response_format) = response_format {
        generation_config.insert(
            "responseMimeType".to_string(),
            Value::String("application/json".to_string()),
        );
        generation_config.insert("responseSchema".to_string(), response_format.schema.clone());
    }
    if let Some(thinking_config) =
        thinking_config_for_request(model, default_reasoning_effort, max_output_tokens)?
    {
        generation_config.insert("thinkingConfig".to_string(), thinking_config);
    }

    if generation_config.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(generation_config)))
    }
}

fn build_tools(tools: &[Value], native_tools: &[ProviderNativeTool]) -> Result<Vec<Value>> {
    let function_declarations = tools
        .iter()
        .map(gemini_function_declaration)
        .collect::<Result<Vec<_>>>()?;

    let mut built_tools = Vec::new();
    let has_function_declarations = !function_declarations.is_empty();
    if has_function_declarations {
        built_tools.push(json!({ "functionDeclarations": function_declarations }));
    } else {
        for tool in native_tools {
            if tool.tool_type == "google_search" {
                built_tools.push(json!({
                    "google_search": tool.config.clone().unwrap_or_else(|| json!({}))
                }));
            }
        }
    }

    Ok(built_tools)
}
