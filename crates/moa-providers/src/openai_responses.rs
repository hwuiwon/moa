//! `OpenAI` Responses API request mapping and stream aggregation helpers.
//!
//! Internal adapter phases:
//! 1. build one provider request from MOA's `CompletionRequest`
//! 2. execute provider transport with retry handling
//! 3. normalize streamed provider events into `CompletionContent`
//! 4. finalize one normalized `CompletionResponse`
//! 5. record provider-private raw response details for tracing/debugging

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use async_openai::Client as OpenAiClient;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::ReasoningEffort;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputContent, InputItem,
    InputParam, InputTextContent, Item, OutputItem, OutputMessageContent, PromptCacheRetention,
    Reasoning, Response, ResponseFormatJsonSchema, ResponseStream, ResponseStreamEvent,
    ResponseTextParam, ResponseUsage, Role as OpenAiRole, Status as OpenAiStatus,
    TextResponseFormatConfiguration, Tool, ToolChoiceOptions, ToolChoiceParam, WebSearchTool,
    WebSearchToolCallStatus,
};
use futures_util::StreamExt;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, JsonResponseFormat,
    MessageRole, MoaError, ModelId, ProviderNativeTool, Result, StopReason, TokenUsage,
    ToolCallContent, ToolContent, ToolInvocation, stable_prefix_fingerprint,
};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::instrumentation::LLMSpanRecorder;
use crate::provider_tools::{web_search_completed_block, web_search_started_block};
use crate::retry::RetryPolicy;
use crate::schema::compile_for_openai_strict;

const OPENAI_METADATA_VALUE_LIMIT: usize = 512;

/// Builds an async-openai client around MOA's shared `OpenAI` configuration.
pub(crate) fn build_openai_client(config: OpenAIConfig) -> OpenAiClient<OpenAIConfig> {
    OpenAiClient::with_config(config)
}

/// Builds one stateless Responses API request from MOA completion inputs.
pub(crate) fn build_responses_request(
    request: &CompletionRequest,
    default_model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<CreateResponse> {
    let mut instructions = Vec::new();
    let mut input_items = Vec::new();

    for message in &request.messages {
        if message.role == MessageRole::System {
            instructions.push(message.content.clone());
            continue;
        }

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
            role: responses_role(message),
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
        .tools
        .iter()
        .map(openai_tool_from_schema)
        .collect::<Result<Vec<_>>>()?;
    tools.extend(openai_native_tools(native_tools)?);
    let has_tools = !tools.is_empty();
    let tools = if tools.is_empty() { None } else { Some(tools) };
    let uses_reasoning_controls = supports_reasoning(default_model);
    let reasoning = if uses_reasoning_controls {
        Some(Reasoning {
            effort: Some(parse_reasoning_effort(default_reasoning_effort)?),
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
        tool_choice: Some(ToolChoiceParam::Mode(if has_tools {
            ToolChoiceOptions::Auto
        } else {
            ToolChoiceOptions::None
        })),
        parallel_tool_calls: has_tools.then_some(true),
        max_output_tokens: request.max_output_tokens.map(|value| value as u32),
        metadata: metadata_as_strings(&request.metadata),
        reasoning,
        stream: Some(true),
        store: Some(false),
        text: request
            .response_format
            .as_ref()
            .map(openai_response_text_param),
        temperature: if uses_reasoning_controls {
            None
        } else {
            request.temperature
        },
        ..CreateResponse::default()
    })
}

fn openai_response_text_param(format: &JsonResponseFormat) -> ResponseTextParam {
    ResponseTextParam {
        format: TextResponseFormatConfiguration::JsonSchema(ResponseFormatJsonSchema {
            description: format.description.clone(),
            name: format.name.clone(),
            schema: Some(format.schema.clone()),
            strict: Some(format.strict),
        }),
        verbosity: None,
    }
}

fn prompt_cache_key(request: &CompletionRequest, model: &str) -> Option<String> {
    let prefix_fingerprint = stable_prefix_fingerprint(request);
    if prefix_fingerprint == 0 {
        return None;
    }

    Some(format!("moa:{model}:{prefix_fingerprint:016x}"))
}

/// Executes one streamed Responses request with retry handling for rate limits.
pub(crate) async fn stream_responses_with_retry(
    client: &OpenAiClient<OpenAIConfig>,
    request: &CreateResponse,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: ModelId,
    started_at: Instant,
    retry_policy: RetryPolicy,
    mut span_recorder: LLMSpanRecorder,
) -> Result<CompletionResponse> {
    let mut attempt = 0usize;

    loop {
        span_recorder.set_phase("transport");
        match client.responses().create_stream(request.clone()).await {
            Ok(stream) => {
                span_recorder.set_phase("stream");
                match consume_responses_stream_once(
                    stream,
                    tx.clone(),
                    fallback_model.clone(),
                    started_at,
                    &mut span_recorder,
                )
                .await
                {
                    Ok(response) => {
                        span_recorder.set_phase("finalize");
                        span_recorder.finish(&response);
                        return Ok(response);
                    }
                    Err(error)
                        if error.retryable
                            && !error.emitted_content
                            && attempt < retry_policy.max_retries =>
                    {
                        let delay = retry_policy.delay_for_attempt(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_retries = retry_policy.max_retries,
                            delay_ms = delay.as_millis(),
                            "provider stream hit a rate limit before any content was emitted; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                    }
                    Err(error) => {
                        span_recorder.fail_at_stage("stream", &error.error);
                        return Err(error.error);
                    }
                }
            }
            Err(error) if is_rate_limit_error(&error) => {
                if attempt >= retry_policy.max_retries {
                    let error = MoaError::RateLimited {
                        retries: retry_policy.max_retries,
                        message: error.to_string(),
                    };
                    span_recorder.fail_at_stage("transport", &error);
                    return Err(error);
                }

                let delay = retry_policy.delay_for_attempt(attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    max_retries = retry_policy.max_retries,
                    delay_ms = delay.as_millis(),
                    "provider request hit a rate limit; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => {
                let error = map_openai_error(error);
                span_recorder.fail_at_stage("transport", &error);
                return Err(error);
            }
        }
    }
}

/// Maps async-openai reasoning-effort strings onto the SDK enum.
pub(crate) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(ReasoningEffort::None),
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::Xhigh),
        other => Err(MoaError::ConfigError(format!(
            "unsupported OpenAI reasoning effort '{other}'"
        ))),
    }
}

async fn consume_responses_stream_once(
    mut stream: ResponseStream,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: ModelId,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> std::result::Result<CompletionResponse, ResponsesStreamError> {
    let mut text = String::new();
    let mut content = Vec::new();
    let mut emitted_function_items = HashSet::new();
    let mut function_items: HashMap<String, FunctionToolCall> = HashMap::new();
    let mut response: Option<Response> = None;
    let mut emitted_content = false;

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) if is_ignorable_openai_stream_error(&error) => continue,
            Err(error) => {
                let retryable = is_rate_limit_error(&error);
                return Err(ResponsesStreamError {
                    error: map_openai_error(error),
                    retryable,
                    emitted_content,
                });
            }
        };
        match event {
            ResponseStreamEvent::ResponseOutputTextDelta(event) => {
                if event.delta.is_empty() {
                    continue;
                }

                text.push_str(&event.delta);
                let block = CompletionContent::Text(event.delta);
                content.push(block.clone());
                span_recorder.observe_block(block.clone());
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseOutputItemAdded(event) => {
                if let OutputItem::FunctionCall(call) = event.item
                    && let Some(item_id) = call.id.clone()
                {
                    function_items.insert(item_id, call);
                }
            }
            ResponseStreamEvent::ResponseOutputItemDone(event) => {
                if let OutputItem::FunctionCall(call) = event.item
                    && let Some(item_id) = call.id.clone()
                {
                    function_items.insert(item_id, call);
                }
            }
            ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
                if emitted_function_items.contains(&event.item_id) {
                    continue;
                }

                let input = parse_tool_arguments(&event.arguments);
                let name = event
                    .name
                    .or_else(|| {
                        function_items
                            .get(&event.item_id)
                            .map(|call| call.name.clone())
                    })
                    .ok_or_else(|| ResponsesStreamError {
                        error: MoaError::ProviderError(format!(
                            "response function call {} did not include a tool name",
                            event.item_id
                        )),
                        retryable: false,
                        emitted_content,
                    })?;
                let call = CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some(event.item_id.clone()),
                        name,
                        input,
                    },
                    provider_metadata: None,
                });
                emitted_function_items.insert(event.item_id);
                content.push(call.clone());
                span_recorder.observe_block(call.clone());
                emitted_content = true;
                if tx.send(Ok(call)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
            | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {
                let block = web_search_started_block();
                content.push(block.clone());
                span_recorder.observe_block(block.clone());
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallCompleted(_) => {
                let block = web_search_completed_block();
                content.push(block.clone());
                span_recorder.observe_block(block.clone());
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseCompleted(event) => {
                response = Some(event.response);
            }
            ResponseStreamEvent::ResponseIncomplete(event) => {
                response = Some(event.response);
            }
            ResponseStreamEvent::ResponseFailed(event) => {
                response = Some(event.response);
            }
            ResponseStreamEvent::ResponseError(event) => {
                return Err(ResponsesStreamError {
                    retryable: is_rate_limit_message(&event.message),
                    emitted_content,
                    error: MoaError::ProviderError(event.message),
                });
            }
            _ => {}
        }
    }

    let response = response.ok_or_else(|| ResponsesStreamError {
        retryable: false,
        emitted_content,
        error: MoaError::ProviderError(
            "Responses stream ended before the provider returned a completed response".to_string(),
        ),
    })?;

    if text.is_empty() {
        text = response_text_from_output(&response.output);
    }

    if content.is_empty() {
        content = response_content_from_output(&response.output).map_err(|error| {
            ResponsesStreamError {
                retryable: false,
                emitted_content,
                error,
            }
        })?;
    }

    let usage = response.usage.clone();
    let token_usage = usage
        .as_ref()
        .map(token_usage_from_openai_usage)
        .unwrap_or_default();
    span_recorder.set_cached_input_tokens(token_usage.input_tokens_cache_read);
    span_recorder.record_raw_response(&response);

    Ok(CompletionResponse {
        text,
        content,
        stop_reason: response_stop_reason(&response),
        model: if response.model.is_empty() {
            fallback_model
        } else {
            ModelId::new(response.model)
        },
        usage: token_usage,
        duration_ms: started_at.elapsed().as_millis() as u64,
        thought_signature: None,
    })
}

fn token_usage_from_openai_usage(usage: &ResponseUsage) -> TokenUsage {
    let cached_input_tokens = usage.input_tokens_details.cached_tokens as usize;
    let input_tokens = usage.input_tokens as usize;
    let output_tokens = usage.output_tokens as usize;

    TokenUsage {
        input_tokens_uncached: input_tokens.saturating_sub(cached_input_tokens),
        input_tokens_cache_write: 0,
        input_tokens_cache_read: cached_input_tokens,
        output_tokens,
    }
}

struct ResponsesStreamError {
    error: MoaError,
    retryable: bool,
    emitted_content: bool,
}

fn openai_native_tools(native_tools: &[ProviderNativeTool]) -> Result<Vec<Tool>> {
    let mut tools = Vec::with_capacity(native_tools.len());
    for tool in native_tools {
        match tool.tool_type.as_str() {
            "web_search" | "web_search_preview" | "web_search_preview_2025_03_11" => {
                tools.push(Tool::WebSearch(WebSearchTool::default()));
            }
            "web_search_2025_08_26" => {
                tools.push(Tool::WebSearch20250826(WebSearchTool::default()));
            }
            other => {
                return Err(MoaError::Unsupported(format!(
                    "unsupported OpenAI native tool '{other}'"
                )));
            }
        }
    }
    Ok(tools)
}

fn map_openai_error(error: OpenAIError) -> MoaError {
    match error {
        OpenAIError::Reqwest(error) => {
            if let Some(status) = error.status() {
                return MoaError::HttpStatus {
                    status: status.as_u16(),
                    retry_after: None,
                    message: error.to_string(),
                };
            }

            MoaError::ProviderError(format!("provider request failed: {error}"))
        }
        OpenAIError::ApiError(error) => MoaError::ProviderError(error.to_string()),
        OpenAIError::JSONDeserialize(error, content) => MoaError::SerializationError(format!(
            "failed to decode provider response: {error}; content: {content}"
        )),
        OpenAIError::FileSaveError(error) | OpenAIError::FileReadError(error) => {
            MoaError::StorageError(error)
        }
        OpenAIError::StreamError(error) => MoaError::StreamError(error.to_string()),
        OpenAIError::InvalidArgument(error) => MoaError::ValidationError(error),
    }
}

fn is_rate_limit_error(error: &OpenAIError) -> bool {
    let generic_message = error.to_string().to_ascii_lowercase();

    match error {
        OpenAIError::Reqwest(error) => error.status() == Some(StatusCode::TOO_MANY_REQUESTS),
        OpenAIError::ApiError(error) => {
            error.code.as_deref() == Some("rate_limit_exceeded")
                || error.message.to_ascii_lowercase().contains("rate limit")
        }
        _ => {
            generic_message.contains("rate limit")
                || generic_message.contains("rate_limit")
                || generic_message.contains("too many requests")
        }
    }
}

fn is_rate_limit_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("too many requests")
}

/// Field names that can appear in `OpenAI` streaming payloads with a type
/// async-openai 0.34 doesn't yet model. We log + skip the chunk instead
/// of letting one quirk tear down the whole session. Add to this list
/// when a new field shows up in production traces.
const IGNORABLE_DESERIALIZE_FIELD_HINTS: &[&str] = &["compatibility", "model_compatibility"];

/// Stream event types that are safe to ignore when the SDK lags behind
/// the provider's event schema.
const IGNORABLE_STREAM_EVENT_TYPES: &[&str] = &["response.rate_limits.updated"];

/// Returns `true` when a streaming or response error is safe to skip
/// past — either an already-known shape (`web_search_call` output items)
/// or a field name on the allow-list above.
///
/// async-openai surfaces unknown-field type mismatches in two shapes:
///   1. `JSONDeserialize(serde_err, content)` — raw serde error + body.
///   2. `InvalidArgument(msg)` — path-aware pre-formatted string like
///      `"compatibility: invalid type: map, expected a string at …"`.
///
/// We inspect both; the allow-list match is on the human-readable
/// message so either shape is covered.
fn is_ignorable_openai_stream_error(error: &OpenAIError) -> bool {
    // Logs the matched field + payload length only — the raw chunk and
    // serde error string can include user prompts, model output, or
    // tool arguments and must not be persisted to logs.
    let field_hint_matches = |text: &str, payload_bytes: Option<usize>| -> bool {
        for hint in IGNORABLE_DESERIALIZE_FIELD_HINTS {
            if text.contains(hint) {
                tracing::warn!(
                    field = hint,
                    payload_bytes = payload_bytes.unwrap_or(0),
                    "openai error skipped due to allow-listed field hint"
                );
                return true;
            }
        }
        false
    };

    let event_type_matches = |text: &str, payload_bytes: Option<usize>| -> bool {
        for event_type in IGNORABLE_STREAM_EVENT_TYPES {
            if text.contains(event_type) {
                tracing::warn!(
                    event_type,
                    payload_bytes = payload_bytes.unwrap_or(0),
                    "openai stream event skipped because the SDK does not model it yet"
                );
                return true;
            }
        }
        false
    };

    match error {
        OpenAIError::JSONDeserialize(serde_err, content) => {
            // Known-safe web_search_call shape (predates the allow-list).
            if content.contains("\"type\":\"response.output_item.")
                && content.contains("\"type\":\"web_search_call\"")
            {
                return true;
            }
            let err_msg = serde_err.to_string();
            let bytes = Some(content.len());
            field_hint_matches(&err_msg, bytes)
                || field_hint_matches(content, bytes)
                || event_type_matches(&err_msg, bytes)
                || event_type_matches(content, bytes)
        }
        OpenAIError::InvalidArgument(msg) => {
            field_hint_matches(msg, None) || event_type_matches(msg, None)
        }
        _ => false,
    }
}

#[cfg(test)]
mod ignorable_error_tests {
    use super::*;

    #[test]
    fn web_search_call_output_item_is_ignorable() {
        // Build a JSONDeserialize error by deliberately failing to
        // deserialize a payload with the web_search_call shape.
        let payload =
            r#"{"type":"response.output_item.added","item":{"type":"web_search_call","id":"x"}}"#;
        let serde_err: serde_json::Error =
            serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn allow_listed_field_in_deserialize_content_is_ignorable() {
        // Real-world shape: the serde error alone doesn't mention the
        // field, but the chunk content does — we match on the content
        // as a second heuristic.
        let payload = r#"{"compatibility": {"foo": "bar"}}"#;
        // Fabricate a cheap serde error for the outer wrapper.
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(
            is_ignorable_openai_stream_error(&err),
            "compatibility-field chunks must be ignorable"
        );
    }

    #[test]
    fn invalid_argument_with_allow_listed_field_is_ignorable() {
        // Mirrors the exact error the user hit: async-openai's
        // path-aware string surfaces as InvalidArgument.
        let msg =
            "compatibility: invalid type: map, expected a string at line 4 column 3".to_string();
        let err = OpenAIError::InvalidArgument(msg);
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn rate_limit_update_event_is_ignorable() {
        let payload = r#"{"type":"response.rate_limits.updated","rate_limits":{"remaining_requests":"14999"}}"#;
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(is_ignorable_openai_stream_error(&err));
    }

    #[test]
    fn unrelated_deserialize_error_is_not_ignorable() {
        let payload = r#"{"foo":"bar"}"#;
        let serde_err = serde_json::from_str::<i32>(payload).expect_err("must fail");
        let err = OpenAIError::JSONDeserialize(serde_err, payload.to_string());
        assert!(!is_ignorable_openai_stream_error(&err));
    }
}

fn responses_role(message: &ContextMessage) -> OpenAiRole {
    match message.role {
        MessageRole::System => OpenAiRole::System,
        MessageRole::User | MessageRole::Tool => OpenAiRole::User,
        MessageRole::Assistant => OpenAiRole::Assistant,
    }
}

fn openai_tool_from_schema(schema: &Value) -> Result<Tool> {
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

fn openai_tool_result_output(message: &ContextMessage) -> FunctionCallOutput {
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

fn parse_tool_arguments(arguments: &str) -> Value {
    match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => Value::String(arguments.to_string()),
    }
}

fn response_text_from_output(output: &[OutputItem]) -> String {
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

fn response_content_from_output(output: &[OutputItem]) -> Result<Vec<CompletionContent>> {
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

fn response_stop_reason(response: &Response) -> StopReason {
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

fn metadata_as_strings(metadata: &HashMap<String, Value>) -> Option<HashMap<String, String>> {
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

fn supports_reasoning(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with('o')
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_openai::error::OpenAIError;
    use async_openai::types::responses::{
        PromptCacheRetention, ResponseUsage, TextResponseFormatConfiguration,
    };
    use moa_core::{
        CacheBreakpoint, CacheTtl, CompletionRequest, ContextMessage, JsonResponseFormat,
    };
    use serde_json::json;

    use super::{
        build_responses_request, is_ignorable_openai_stream_error, metadata_as_strings,
        token_usage_from_openai_usage,
    };

    #[test]
    fn metadata_as_strings_drops_internal_moa_keys() {
        let metadata = HashMap::from([
            ("_moa.session_id".to_string(), json!("session-123")),
            ("visible".to_string(), json!("value")),
        ]);

        let filtered = metadata_as_strings(&metadata).expect("filtered metadata");

        assert_eq!(filtered.get("visible").map(String::as_str), Some("value"));
        assert!(!filtered.contains_key("_moa.session_id"));
    }

    #[test]
    fn ignores_web_search_output_done_incomplete_status() {
        let decode_error = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let error = OpenAIError::JSONDeserialize(
            decode_error,
            "{\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\",\"status\":\"incomplete\"}}".to_string(),
        );

        assert!(is_ignorable_openai_stream_error(&error));
    }

    #[test]
    fn token_usage_from_openai_usage_splits_cached_prompt_tokens() {
        let usage: ResponseUsage = serde_json::from_value(json!({
            "input_tokens": 2048,
            "output_tokens": 512,
            "total_tokens": 2560,
            "input_tokens_details": {
                "cached_tokens": 1536
            },
            "output_tokens_details": {
                "reasoning_tokens": 0
            }
        }))
        .expect("usage fixture should deserialize");

        let token_usage = token_usage_from_openai_usage(&usage);
        assert_eq!(token_usage.input_tokens_uncached, 512);
        assert_eq!(token_usage.input_tokens_cache_write, 0);
        assert_eq!(token_usage.input_tokens_cache_read, 1536);
        assert_eq!(token_usage.output_tokens, 512);
    }

    #[test]
    fn build_responses_request_sets_prompt_cache_key_and_retention() {
        let request = CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("Static instructions".to_string()),
                ContextMessage::user("Current task".to_string()),
            ],
            tools: vec![json!({
                "name": "echo",
                "description": "Echo tool",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "value": { "type": "string" }
                    }
                }
            })],
            max_output_tokens: Some(128),
            temperature: None,
            response_format: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
            metadata: HashMap::new(),
        };

        let built = build_responses_request(&request, "gpt-5.4", "medium", &[])
            .expect("request should build");

        assert_eq!(
            built.prompt_cache_retention,
            Some(PromptCacheRetention::InMemory)
        );
        assert!(
            built
                .prompt_cache_key
                .as_deref()
                .is_some_and(|key| key.starts_with("moa:gpt-5.4:")),
            "expected a stable OpenAI prompt cache key"
        );
    }

    #[test]
    fn build_responses_request_omits_temperature_for_reasoning_models() {
        let request = CompletionRequest {
            model: None,
            messages: vec![ContextMessage::user("Rewrite this query")],
            tools: Vec::new(),
            max_output_tokens: Some(128),
            temperature: Some(0.0),
            response_format: None,
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            metadata: HashMap::new(),
        };

        let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
            .expect("request should build");

        assert_eq!(built.temperature, None);
    }

    #[test]
    fn build_responses_request_sets_structured_output_schema() {
        let mut request = CompletionRequest::new("Return structured data.");
        request.response_format = Some(JsonResponseFormat::strict_json_schema(
            "test_payload",
            "Test payload.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "answer": { "type": "string" }
                },
                "required": ["answer"]
            }),
        ));

        let built = build_responses_request(&request, "gpt-5.4-mini", "medium", &[])
            .expect("request should build");
        let text = built.text.expect("structured output text config");
        let TextResponseFormatConfiguration::JsonSchema(schema) = text.format else {
            panic!("expected json_schema text format");
        };

        assert_eq!(schema.name, "test_payload");
        assert_eq!(schema.strict, Some(true));
        assert_eq!(
            schema
                .schema
                .and_then(|schema| schema.get("required").cloned()),
            Some(json!(["answer"]))
        );
    }

    #[test]
    fn prompt_cache_key_ignores_dynamic_tail_messages() {
        let mut first = CompletionRequest {
            model: None,
            messages: vec![
                ContextMessage::system("Static instructions".to_string()),
                ContextMessage::user("Tail one".to_string()),
            ],
            tools: Vec::new(),
            max_output_tokens: Some(128),
            temperature: None,
            response_format: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
            metadata: HashMap::new(),
        };
        let mut second = first.clone();
        first
            .messages
            .push(ContextMessage::assistant("Dynamic assistant A"));
        second
            .messages
            .push(ContextMessage::assistant("Dynamic assistant B"));

        let first_built =
            build_responses_request(&first, "gpt-5.4", "medium", &[]).expect("first request");
        let second_built =
            build_responses_request(&second, "gpt-5.4", "medium", &[]).expect("second request");

        assert_eq!(first_built.prompt_cache_key, second_built.prompt_cache_key);
    }
}
