//! Gemini request construction and explicit cache planning.

use super::response::GeminiCachedContent;
use super::tools::{
    content_message, flush_pending_parts, flush_tool_responses, function_call_part,
    function_response_part, gemini_function_declaration, is_standard_user_message, text_part,
    thinking_config_for_model,
};
use super::*;

impl GeminiProvider {
    pub(super) async fn build_request_body_with_cache(
        &self,
        request: &CompletionRequest,
        model: &str,
        default_reasoning_effort: &str,
        native_tools: &[ProviderNativeTool],
    ) -> Result<Value> {
        if let Some(plan) =
            build_explicit_cache_plan(request, model, default_reasoning_effort, native_tools)?
        {
            match self.cached_content_name(&plan).await {
                Ok(cache_name) => {
                    return build_request_body_from_parts(plan.tail_parts, Some(cache_name));
                }
                Err(error) => {
                    tracing::warn!(
                        model,
                        error = %error,
                        "falling back to inline Gemini prompt after explicit cache setup failed"
                    );
                }
            }
        }

        build_request_body(request, model, default_reasoning_effort, native_tools)
    }

    async fn cached_content_name(&self, plan: &GeminiExplicitCachePlan) -> Result<String> {
        if let Some(name) = self
            .explicit_cache_names
            .lock()
            .await
            .get(&plan.cache_key)
            .cloned()
        {
            return Ok(name);
        }

        let response = self
            .client
            .post(format!(
                "{}/cachedContents",
                self.api_base.trim_end_matches('/')
            ))
            .header("x-goog-api-key", &*self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .json(&plan.cache_body)
            .send()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unavailable>".to_string());
            return Err(MoaError::ProviderError(format!(
                "Gemini cachedContents.create failed with {status}: {body}"
            )));
        }

        let created: GeminiCachedContent = response
            .json()
            .await
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        let mut cache_names = self.explicit_cache_names.lock().await;
        let entry = cache_names
            .entry(plan.cache_key.clone())
            .or_insert_with(|| created.name.clone());
        Ok(entry.clone())
    }
}

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

pub(super) struct GeminiExplicitCachePlan {
    cache_key: String,
    cache_body: Value,
    tail_parts: GeminiRequestParts,
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
    build_request_body_from_parts(parts, None)
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
    cached_content_name: Option<String>,
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
    if let Some(cached_content_name) = cached_content_name {
        body.insert(
            "cachedContent".to_string(),
            Value::String(cached_content_name),
        );
    }

    Ok(Value::Object(body))
}

fn build_contents_from_messages(messages: &[ContextMessage]) -> (Option<Value>, Vec<Value>) {
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut model_parts = Vec::new();
    let mut tool_response_parts = Vec::new();
    let mut tool_names_by_id = HashMap::new();

    for message in messages {
        if message.role == MessageRole::System {
            flush_pending_parts(&mut contents, &mut model_parts, &mut tool_response_parts);
            if !message.content.is_empty() || message.thought_signature.is_some() {
                system_parts.push(text_part(
                    message.content.as_str(),
                    message.thought_signature.as_deref(),
                ));
            }
            continue;
        }

        if is_standard_user_message(message) {
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
    if let Some(thinking_config) = thinking_config_for_model(model, default_reasoning_effort)? {
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

pub(super) fn build_explicit_cache_plan(
    request: &CompletionRequest,
    model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<Option<GeminiExplicitCachePlan>> {
    let Some((message_count, ttl)) = deepest_cache_boundary(request) else {
        return Ok(None);
    };
    let message_count = message_count.min(request.messages.len());
    if message_count == 0 || message_count >= request.messages.len() {
        return Ok(None);
    }

    let prefix_tokens = estimate_prefix_tokens(request, message_count);
    if prefix_tokens < minimum_cacheable_tokens(model) {
        return Ok(None);
    }

    let prefix_parts = build_request_parts(
        &request.messages[..message_count],
        &request.tools,
        GeminiRequestBuildOptions {
            model,
            max_output_tokens: request.max_output_tokens,
            temperature: request.temperature,
            response_format: None,
            default_reasoning_effort,
            native_tools,
            include_tools: true,
        },
    )?;
    let tail_parts = build_request_parts(
        &request.messages[message_count..],
        &[],
        GeminiRequestBuildOptions {
            model,
            max_output_tokens: request.max_output_tokens,
            temperature: request.temperature,
            response_format: request.response_format.as_ref(),
            default_reasoning_effort,
            native_tools: &[],
            include_tools: false,
        },
    )?;
    if tail_parts.contents.is_empty() {
        return Ok(None);
    }
    if tail_parts.system_instruction.is_some() {
        return Ok(None);
    }

    let cache_body = build_cache_create_body(model, ttl, prefix_parts);
    let cache_key = format!(
        "{model}:{message_count}:{:016x}",
        fingerprint_cache_body(&cache_body)
    );

    Ok(Some(GeminiExplicitCachePlan {
        cache_key,
        cache_body,
        tail_parts,
    }))
}

fn build_cache_create_body(model: &str, ttl: CacheTtl, parts: GeminiRequestParts) -> Value {
    let mut body = Map::new();
    body.insert(
        "model".to_string(),
        Value::String(format!("models/{model}")),
    );
    body.insert(
        "displayName".to_string(),
        Value::String(format!("moa-{}", ttl.as_anthropic_ttl())),
    );
    body.insert(
        "ttl".to_string(),
        Value::String(match ttl {
            CacheTtl::FiveMinutes => "300s".to_string(),
            CacheTtl::OneHour => "3600s".to_string(),
        }),
    );
    if let Some(system_instruction) = parts.system_instruction {
        body.insert("systemInstruction".to_string(), system_instruction);
    }
    if !parts.contents.is_empty() {
        body.insert("contents".to_string(), Value::Array(parts.contents));
    }
    if !parts.tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(parts.tools));
    }

    Value::Object(body)
}

fn deepest_cache_boundary(request: &CompletionRequest) -> Option<(usize, CacheTtl)> {
    let explicit = request
        .cache_controls
        .iter()
        .filter_map(|breakpoint| {
            breakpoint
                .message_index()
                .map(|index| (index, breakpoint.ttl))
        })
        .max_by_key(|(index, _)| *index);
    if explicit.is_some() {
        return explicit;
    }

    request
        .cache_breakpoints
        .iter()
        .copied()
        .max()
        .map(|index| (index, CacheTtl::OneHour))
}

fn estimate_prefix_tokens(request: &CompletionRequest, message_count: usize) -> usize {
    let message_tokens = request.messages[..message_count]
        .iter()
        .map(|message| moa_core::estimate_text_tokens(&message.content))
        .sum::<usize>();
    let tool_tokens = request
        .tools
        .iter()
        .map(|tool| moa_core::estimate_text_tokens(&tool.to_string()))
        .sum::<usize>();
    message_tokens + tool_tokens
}

fn minimum_cacheable_tokens(model: &str) -> usize {
    if model.starts_with("gemini-2.5-pro") || model.starts_with("gemini-3.1-pro") {
        return 4_096;
    }
    1_024
}

fn fingerprint_cache_body(value: &Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let serialized = serde_json::to_string(value).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}
