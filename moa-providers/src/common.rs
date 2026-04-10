//! Shared HTTP and SSE utilities for provider implementations.

use std::collections::{HashMap, HashSet};
use std::time::Duration;
use std::time::Instant;

use async_openai::Client as OpenAiClient;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::ReasoningEffort;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionTool, FunctionToolCall, InputItem,
    InputParam, OutputItem, OutputMessageContent, Reasoning, Response, ResponseStream,
    ResponseStreamEvent, Role as OpenAiRole, Status as OpenAiStatus, Tool, ToolChoiceOptions,
    ToolChoiceParam,
};
use eventsource_stream::Event as SseEvent;
use futures_util::StreamExt;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, MessageRole,
    StopReason, ToolInvocation,
};
use moa_core::{MoaError, Result};
use reqwest::{Client, RequestBuilder, Response as HttpResponse, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

const INITIAL_RETRY_DELAY_MS: u64 = 250;
const OPENAI_METADATA_VALUE_LIMIT: usize = 512;

/// Builds the shared HTTP client used by provider implementations.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}

/// Sends a request with retry handling for provider rate limits.
pub(crate) async fn send_with_retry<F>(build_request: F, max_retries: usize) -> Result<HttpResponse>
where
    F: Fn() -> RequestBuilder,
{
    let mut attempt = 0usize;

    loop {
        let response = build_request().send().await.map_err(|error| {
            MoaError::ProviderError(format!("provider request failed: {error}"))
        })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let message = response_text(response).await;
            if attempt >= max_retries {
                return Err(MoaError::RateLimited {
                    retries: max_retries,
                    message,
                });
            }

            let delay = retry_delay(attempt);
            tracing::warn!(
                attempt = attempt + 1,
                max_retries,
                delay_ms = delay.as_millis(),
                "provider request hit a rate limit; retrying"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
            continue;
        }

        if !status.is_success() {
            let message = response_text(response).await;
            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                message,
            });
        }

        return Ok(response);
    }
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

/// Returns the exponential backoff delay for a retry attempt.
pub(crate) fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(INITIAL_RETRY_DELAY_MS.saturating_mul(1_u64 << attempt.min(8)))
}

async fn response_text(response: HttpResponse) -> String {
    match response.text().await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "request failed with an empty response body".to_string(),
        Err(error) => format!("request failed and the response body could not be read: {error}"),
    }
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
) -> Result<CreateResponse> {
    let mut instructions = Vec::new();
    let mut input_items = Vec::new();

    for message in &request.messages {
        if message.role == MessageRole::System {
            instructions.push(message.content.clone());
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

    let tools = if request.tools.is_empty() {
        None
    } else {
        Some(
            request
                .tools
                .iter()
                .map(openai_tool_from_schema)
                .collect::<Result<Vec<_>>>()?,
        )
    };
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
        tool_choice: request
            .tools
            .is_empty()
            .then_some(ToolChoiceParam::Mode(ToolChoiceOptions::None))
            .or(Some(ToolChoiceParam::Mode(ToolChoiceOptions::Auto))),
        parallel_tool_calls: (!request.tools.is_empty()).then_some(true),
        max_output_tokens: request.max_output_tokens.map(|value| value as u32),
        metadata: metadata_as_strings(&request.metadata),
        reasoning,
        stream: Some(true),
        store: Some(false),
        temperature: request.temperature,
        ..CreateResponse::default()
    })
}

/// Executes one streamed Responses request with retry handling for rate limits.
pub(crate) async fn stream_responses_with_retry(
    client: &OpenAiClient<OpenAIConfig>,
    request: &CreateResponse,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    max_retries: usize,
) -> Result<CompletionResponse> {
    let mut attempt = 0usize;

    loop {
        match client.responses().create_stream(request.clone()).await {
            Ok(stream) => match consume_responses_stream_once(
                stream,
                tx.clone(),
                fallback_model.clone(),
                started_at,
            )
            .await
            {
                Ok(response) => return Ok(response),
                Err(error)
                    if error.retryable && !error.emitted_content && attempt < max_retries =>
                {
                    let delay = retry_delay(attempt);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_retries,
                        delay_ms = delay.as_millis(),
                        "provider stream hit a rate limit before any content was emitted; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error.error),
            },
            Err(error) if is_rate_limit_error(&error) => {
                if attempt >= max_retries {
                    return Err(MoaError::RateLimited {
                        retries: max_retries,
                        message: error.to_string(),
                    });
                }

                let delay = retry_delay(attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    max_retries,
                    delay_ms = delay.as_millis(),
                    "provider request hit a rate limit; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(error) => return Err(map_openai_error(error)),
        }
    }
}

/// Consumes a typed Responses API stream into MOA streaming blocks and a final response.
async fn consume_responses_stream_once(
    mut stream: ResponseStream,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
) -> std::result::Result<CompletionResponse, ResponsesStreamError> {
    let mut text = String::new();
    let mut content = Vec::new();
    let mut emitted_function_items = HashSet::new();
    let mut function_items: HashMap<String, FunctionToolCall> = HashMap::new();
    let mut response: Option<Response> = None;
    let mut emitted_content = false;

    while let Some(event) = stream.next().await {
        let event = event.map_err(|error| {
            let retryable = is_rate_limit_error(&error);
            ResponsesStreamError {
                error: map_openai_error(error),
                retryable,
                emitted_content,
            }
        })?;
        match event {
            ResponseStreamEvent::ResponseOutputTextDelta(event) => {
                if event.delta.is_empty() {
                    continue;
                }

                text.push_str(&event.delta);
                let block = CompletionContent::Text(event.delta);
                content.push(block.clone());
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
                let call = CompletionContent::ToolCall(ToolInvocation {
                    id: Some(event.item_id.clone()),
                    name,
                    input,
                });
                emitted_function_items.insert(event.item_id);
                content.push(call.clone());
                emitted_content = true;
                if tx.send(Ok(call)).await.is_err() {
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

fn responses_role(message: &ContextMessage) -> OpenAiRole {
    match message.role {
        MessageRole::System => OpenAiRole::System,
        MessageRole::User | MessageRole::Tool => OpenAiRole::User,
        MessageRole::Assistant => OpenAiRole::Assistant,
    }
}

fn openai_tool_from_schema(schema: &Value) -> Result<Tool> {
    if let Some(function) = schema.get("function").and_then(Value::as_object) {
        return build_function_tool(
            function.get("name"),
            function.get("description"),
            function.get("parameters"),
        );
    }

    build_function_tool(
        schema.get("name"),
        schema.get("description"),
        schema
            .get("parameters")
            .or_else(|| schema.get("input_schema")),
    )
}

fn build_function_tool(
    name: Option<&Value>,
    description: Option<&Value>,
    parameters: Option<&Value>,
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
        strict: Some(false),
        description,
        defer_loading: None,
    }))
}

fn parse_tool_arguments(arguments: &str) -> Result<Value> {
    match serde_json::from_str(arguments) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(arguments.to_string())),
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
                content.push(CompletionContent::ToolCall(ToolInvocation {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    input: parse_tool_arguments(&call.arguments)?,
                }));
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

fn metadata_as_strings(metadata: &HashMap<String, Value>) -> Option<HashMap<String, String>> {
    if metadata.is_empty() {
        return None;
    }

    let filtered: HashMap<String, String> = metadata
        .iter()
        .filter_map(|(key, value)| {
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use reqwest::StatusCode;

    use super::{build_http_client, send_with_retry};

    #[tokio::test]
    async fn retries_on_rate_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let current = request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;

                let response = if current == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 11\r\n\r\nrate limit"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                };

                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = build_http_client().unwrap();
        let url = format!("http://{address}/retry");
        let response = send_with_retry(|| client.get(&url), 3).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        server.abort();
    }
}
