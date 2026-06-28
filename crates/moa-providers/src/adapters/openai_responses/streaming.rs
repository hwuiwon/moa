//! OpenAI Responses streaming and retry handling.

use super::response::{ResponsesStreamError, token_usage_from_openai_usage};
use super::tools::{
    parse_tool_arguments, response_content_from_output, response_stop_reason,
    response_text_from_output,
};
use super::*;
use crate::core::provider_tools::{web_search_completed_block, web_search_started_block};

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
                        if error.rate_limited {
                            // Match the shared RetryPolicy: an exhausted rate
                            // limit is a typed RateLimited, never a generic
                            // ProviderError or HttpStatus{429}.
                            let message = match error.error {
                                MoaError::RateLimited { message, .. }
                                | MoaError::ProviderError(message) => message,
                                other => other.to_string(),
                            };
                            return Err(MoaError::RateLimited {
                                retries: retry_policy.max_retries,
                                message,
                            });
                        }
                        return Err(error.error);
                    }
                }
            }
            Err(error) if is_retryable_openai_error(&error) => {
                if attempt >= retry_policy.max_retries {
                    let error = if is_rate_limit_error(&error) {
                        MoaError::RateLimited {
                            retries: retry_policy.max_retries,
                            message: error.to_string(),
                        }
                    } else {
                        map_openai_error(error)
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
pub(super) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort> {
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
                let retryable = is_retryable_openai_error(&error);
                let rate_limited = is_rate_limit_error(&error);
                return Err(ResponsesStreamError {
                    error: map_openai_error(error),
                    retryable,
                    emitted_content,
                    rate_limited,
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
                        rate_limited: false,
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
            ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
            | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {
                let block = web_search_started_block();
                content.push(block.clone());
                span_recorder.observe_block(&block);
                emitted_content = true;
                if tx.send(Ok(block)).await.is_err() {
                    break;
                }
            }
            ResponseStreamEvent::ResponseWebSearchCallCompleted(_) => {
                let block = web_search_completed_block();
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
                let rate_limited = is_rate_limit_message(&event.message);
                return Err(ResponsesStreamError {
                    retryable: rate_limited,
                    emitted_content,
                    rate_limited,
                    error: MoaError::ProviderError(event.message),
                });
            }
            _ => {}
        }
    }

    let response = response.ok_or_else(|| ResponsesStreamError {
        retryable: false,
        emitted_content,
        rate_limited: false,
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
                rate_limited: false,
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

pub(super) fn openai_native_tools(native_tools: &[ProviderNativeTool]) -> Result<Vec<Tool>> {
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
        OpenAIError::ApiError(error) if is_server_error_api_error(&error) => MoaError::HttpStatus {
            status: 500,
            retry_after: None,
            message: error.to_string(),
        },
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

fn is_retryable_openai_error(error: &OpenAIError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    is_rate_limit_error(error)
        || match error {
            OpenAIError::Reqwest(error) => error
                .status()
                .is_some_and(|status| status.is_server_error()),
            OpenAIError::ApiError(error) => is_server_error_api_error(error),
            _ => false,
        }
        || message.contains("server_error")
        || message.contains("upstream unavailable")
        || message.contains("internal server error")
        || message.contains("status 500")
        || message.contains("http 500")
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

fn is_server_error_api_error(error: &async_openai::error::ApiError) -> bool {
    error
        .r#type
        .as_deref()
        .is_some_and(|kind| kind.contains("server") || kind.contains("temporar"))
        || error
            .code
            .as_deref()
            .is_some_and(|code| code.contains("server") || code.contains("temporar"))
        || error.message.to_ascii_lowercase().contains("server error")
        || error.message.to_ascii_lowercase().contains("server_error")
        || error
            .message
            .to_ascii_lowercase()
            .contains("upstream unavailable")
        || error
            .message
            .to_ascii_lowercase()
            .contains("temporarily unavailable")
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
pub(super) fn is_ignorable_openai_stream_error(error: &OpenAIError) -> bool {
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
