//! Shared HTTP and SSE utilities for provider implementations.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use async_openai::Client as OpenAiClient;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::ReasoningEffort;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputContent, InputItem,
    InputParam, InputTextContent, Item, OutputItem, OutputMessageContent, Reasoning, Response,
    ResponseStream, ResponseStreamEvent, Role as OpenAiRole, Status as OpenAiStatus, Tool,
    ToolChoiceOptions, ToolChoiceParam, WebSearchTool, WebSearchToolCallStatus,
};
use eventsource_stream::Event as SseEvent;
use futures_util::StreamExt;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, MessageRole,
    ProviderNativeTool, StopReason, ToolCallContent, ToolContent, ToolInvocation,
};
use moa_core::{MoaError, Result};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::instrumentation::LLMSpanRecorder;
use crate::retry::RetryPolicy;
use crate::schema::compile_for_openai_strict;

const OPENAI_METADATA_VALUE_LIMIT: usize = 512;

/// Builds the shared HTTP client used by provider implementations.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}

/// Parses a JSON SSE payload into a strongly typed Rust value.
pub(crate) fn parse_sse_json<T>(event: &SseEvent) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&event.data).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to parse SSE payload for event '{}': {error}",
            event.event
        ))
    })
}

/// Builds an async-openai client around MOA's shared HTTP client.
pub(crate) fn build_openai_client(config: OpenAIConfig) -> Result<OpenAiClient<OpenAIConfig>> {
    Ok(OpenAiClient::with_config(config))
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
    let reasoning = if supports_reasoning(default_model) {
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
        temperature: request.temperature,
        ..CreateResponse::default()
    })
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

/// Executes one streamed Responses request with retry handling for rate limits.
pub(crate) async fn stream_responses_with_retry(
    client: &OpenAiClient<OpenAIConfig>,
    request: &CreateResponse,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    retry_policy: RetryPolicy,
    mut span_recorder: LLMSpanRecorder,
) -> Result<CompletionResponse> {
    let mut attempt = 0usize;

    loop {
        match client.responses().create_stream(request.clone()).await {
            Ok(stream) => match consume_responses_stream_once(
                stream,
                tx.clone(),
                fallback_model.clone(),
                started_at,
                &mut span_recorder,
            )
            .await
            {
                Ok(response) => {
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
                    span_recorder.fail(&error.error);
                    return Err(error.error);
                }
            },
            Err(error) if is_rate_limit_error(&error) => {
                if attempt >= retry_policy.max_retries {
                    let error = MoaError::RateLimited {
                        retries: retry_policy.max_retries,
                        message: error.to_string(),
                    };
                    span_recorder.fail(&error);
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
                span_recorder.fail(&error);
                return Err(error);
            }
        }
    }
}

/// Consumes a typed Responses API stream into MOA streaming blocks and a final response.
async fn consume_responses_stream_once(
    mut stream: ResponseStream,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
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
                span_recorder.observe_block(&block);
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

                let input = parse_tool_arguments(&event.arguments).map_err(|error| {
                    ResponsesStreamError {
                        error,
                        retryable: false,
                        emitted_content,
                    }
                })?;
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
                span_recorder.observe_block(&call);
                emitted_content = true;
                if tx.send(Ok(call)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallInProgress(_) => {
                let block = CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: "Searching the web...".to_string(),
                };
                content.push(block.clone());
                span_recorder.observe_block(&block);
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {
                let block = CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: "Searching the web...".to_string(),
                };
                content.push(block.clone());
                span_recorder.observe_block(&block);
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallCompleted(_) => {
                let block = CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: "Web search completed.".to_string(),
                };
                content.push(block.clone());
                span_recorder.observe_block(&block);
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
    let cached_input_tokens = usage
        .as_ref()
        .map(|usage| usage.input_tokens_details.cached_tokens as usize)
        .unwrap_or(0);
    span_recorder.set_cached_input_tokens(cached_input_tokens);
    let input_tokens = usage
        .as_ref()
        .map(|usage| usage.input_tokens as usize)
        .unwrap_or(0);
    let output_tokens = usage
        .as_ref()
        .map(|usage| usage.output_tokens as usize)
        .unwrap_or(0);

    Ok(CompletionResponse {
        text,
        content,
        stop_reason: response_stop_reason(&response),
        model: if response.model.is_empty() {
            fallback_model
        } else {
            response.model
        },
        input_tokens,
        output_tokens,
        cached_input_tokens,
        duration_ms: started_at.elapsed().as_millis() as u64,
        thought_signature: None,
    })
}

struct ResponsesStreamError {
    error: MoaError,
    retryable: bool,
    emitted_content: bool,
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

pub(crate) fn map_openai_error(error: OpenAIError) -> MoaError {
    match error {
        OpenAIError::Reqwest(error) => {
            if let Some(status) = error.status() {
                return MoaError::HttpStatus {
                    status: status.as_u16(),
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

fn is_ignorable_openai_stream_error(error: &OpenAIError) -> bool {
    match error {
        OpenAIError::JSONDeserialize(_, content) => {
            content.contains("\"type\":\"response.output_item.")
                && content.contains("\"type\":\"web_search_call\"")
        }
        _ => false,
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

pub(crate) fn parse_tool_arguments(arguments: &str) -> Result<Value> {
    match serde_json::from_str(arguments) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(arguments.to_string())),
    }
}

pub(crate) fn response_text_from_output(output: &[OutputItem]) -> String {
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

pub(crate) fn response_content_from_output(
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
                        input: parse_tool_arguments(&call.arguments)?,
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

pub(crate) fn response_stop_reason(response: &Response) -> StopReason {
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

    use serde_json::json;

    use async_openai::error::OpenAIError;

    use super::{is_ignorable_openai_stream_error, metadata_as_strings};

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
}
