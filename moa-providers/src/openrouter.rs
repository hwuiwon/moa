//! OpenRouter provider implementation using the OpenResponses-compatible API.

use std::collections::{HashMap, HashSet};
use std::env;
use std::time::Instant;

use async_openai::types::responses::{FunctionToolCall, OutputItem, Response};
use futures_util::TryStreamExt;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    MoaConfig, MoaError, ModelCapabilities, ProviderNativeTool, Result, TokenPricing,
    ToolCallFormat,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::{Map, Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use tokio_util::io::StreamReader;
use tracing::Instrument;

use crate::common::{
    build_http_client, build_responses_request, parse_tool_arguments, response_content_from_output,
    response_stop_reason, response_text_from_output, send_with_retry,
};
use crate::instrumentation::LLMSpanRecorder;
use crate::openai::{
    canonical_model_id as canonical_openai_model_id,
    capabilities_for_model as openai_capabilities_for_model,
};

const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_RETRIES: usize = 3;
const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_HTTP_REFERER: &str = "https://github.com/hwuiwon/moa";
const OPENROUTER_TITLE: &str = "MOA";

/// OpenRouter provider backed by the `/responses` OpenResponses-compatible API.
pub struct OpenRouterProvider {
    client: reqwest::Client,
    api_base: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    api_key: String,
    max_retries: usize,
    web_search_enabled: bool,
}

impl OpenRouterProvider {
    /// Creates a provider from an API key and default model identifier.
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Result<Self> {
        Self::new_with_reasoning_effort(api_key, default_model, "medium")
    }

    /// Creates a provider from an API key, default model, and default reasoning effort.
    pub fn new_with_reasoning_effort(
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        default_reasoning_effort: impl Into<String>,
    ) -> Result<Self> {
        let default_model = canonical_model_id(&default_model.into());
        let default_capabilities = capabilities_for_model(&default_model);
        let api_key = api_key.into();

        Ok(Self {
            client: build_http_client()?,
            api_base: OPENROUTER_API_BASE.to_string(),
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            api_key,
            max_retries: DEFAULT_MAX_RETRIES,
            web_search_enabled: true,
        })
    }

    /// Creates a provider from the configured OpenRouter environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.general.default_model.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key_env = config.providers.openrouter.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;

        Self::new_with_reasoning_effort(
            api_key,
            default_model,
            config.general.reasoning_effort.clone(),
        )
        .map(|provider| provider.with_web_search_enabled(config.general.web_search_enabled))
    }

    /// Creates a provider from the `OPENROUTER_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("OPENROUTER_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("OPENROUTER_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Result<Self> {
        self.api_base = api_base.into();
        Ok(self)
    }

    /// Overrides the retry budget for rate-limited requests.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Overrides whether provider-native web search is exposed to supported models.
    pub fn with_web_search_enabled(mut self, enabled: bool) -> Self {
        self.web_search_enabled = enabled;
        self
    }
}

#[async_trait::async_trait]
impl LLMProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.default_capabilities.clone()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let requested_model = request
            .model
            .as_deref()
            .unwrap_or(self.default_model.as_str())
            .to_string();
        let resolved_model = canonical_model_id(&requested_model);
        let model_capabilities = capabilities_for_model(&resolved_model);
        let request_model = resolved_model.clone();
        let span_recorder = LLMSpanRecorder::new(
            "openrouter",
            request_model.clone(),
            &request,
            request.max_output_tokens,
            model_capabilities.pricing.clone(),
        );
        let span = span_recorder.span().clone();
        let request_body = match build_request_body(
            &request,
            &request_model,
            &self.default_reasoning_effort,
            native_tools(&model_capabilities, self.web_search_enabled),
        ) {
            Ok(request) => request,
            Err(error) => {
                span_recorder.fail(&error);
                return Err(error);
            }
        };
        let client = self.client.clone();
        let api_base = self.api_base.clone();
        let api_key = self.api_key.clone();
        let max_retries = self.max_retries;
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                let started_at = Instant::now();
                let mut span_recorder = span_recorder;
                let response = send_with_retry(
                    || {
                        client
                            .post(format!("{}/responses", api_base.trim_end_matches('/')))
                            .bearer_auth(&api_key)
                            .header("HTTP-Referer", OPENROUTER_HTTP_REFERER)
                            .header("X-Title", OPENROUTER_TITLE)
                            .header(ACCEPT, "text/event-stream")
                            .header(CONTENT_TYPE, "application/json")
                            .json(&request_body)
                    },
                    max_retries,
                )
                .await;

                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        span_recorder.fail(&error);
                        return Err(error);
                    }
                };

                let response =
                    consume_sse_events(response, tx, request_model, started_at, &mut span_recorder)
                        .await;

                match response {
                    Ok(response) => {
                        span_recorder.finish(&response);
                        Ok(response)
                    }
                    Err(error) => {
                        span_recorder.fail(&error);
                        Err(error)
                    }
                }
            }
            .instrument(span),
        );

        Ok(CompletionStream::new(rx, completion_task))
    }
}

fn canonical_model_id(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }

    if model.starts_with("claude-") {
        return format!("anthropic/{model}");
    }

    if canonical_openai_model_id(model).is_ok() {
        return format!("openai/{model}");
    }

    model.to_string()
}

fn capabilities_for_model(model: &str) -> ModelCapabilities {
    let providerless_model = model.split('/').nth(1).unwrap_or(model);

    if let Ok(capabilities) = openai_capabilities_for_model(providerless_model) {
        return ModelCapabilities {
            model_id: model.to_string(),
            native_tools: native_web_search_tools(),
            ..capabilities
        };
    }

    if providerless_model.starts_with("claude-opus-4-6") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 5.0,
                output_per_mtok: 25.0,
                cached_input_per_mtok: None,
            },
            native_tools: native_web_search_tools(),
        };
    }

    if providerless_model.starts_with("claude-sonnet-4-6") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: None,
            },
            native_tools: native_web_search_tools(),
        };
    }

    ModelCapabilities {
        model_id: model.to_string(),
        context_window: 128_000,
        max_output: 16_384,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<Value> {
    let base_request = build_responses_request(request, model, default_reasoning_effort, &[])?;
    let mut body = serde_json::to_value(base_request).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to serialize OpenRouter Responses request: {error}"
        ))
    })?;
    normalize_openrouter_input_format(&mut body)?;

    if native_tools.is_empty() {
        return Ok(body);
    }

    let object = body.as_object_mut().ok_or_else(|| {
        MoaError::SerializationError(
            "serialized OpenRouter Responses request was not a JSON object".to_string(),
        )
    })?;

    let tools = object
        .entry("tools".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let tools = tools.as_array_mut().ok_or_else(|| {
        MoaError::SerializationError(
            "serialized OpenRouter Responses request tools field was not an array".to_string(),
        )
    })?;
    tools.extend(native_tools.iter().map(provider_native_tool_json));

    if !tools.is_empty() {
        object.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        object.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        object.remove("reasoning");
    }

    Ok(body)
}

fn native_web_search_tools() -> Vec<ProviderNativeTool> {
    vec![ProviderNativeTool {
        tool_type: "openrouter:web_search".to_string(),
        name: "web_search".to_string(),
        config: Some(json!({
            "parameters": {
                "engine": "auto",
            }
        })),
    }]
}

fn provider_native_tool_json(tool: &ProviderNativeTool) -> Value {
    let mut value = Map::new();
    value.insert("type".to_string(), Value::String(tool.tool_type.clone()));
    if !tool.name.is_empty() && !tool.tool_type.starts_with("openrouter:") {
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

fn normalize_openrouter_input_format(body: &mut Value) -> Result<()> {
    let object = body.as_object_mut().ok_or_else(|| {
        MoaError::SerializationError(
            "serialized OpenRouter Responses request was not a JSON object".to_string(),
        )
    })?;

    let input = object
        .get_mut("input")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            MoaError::SerializationError(
                "serialized OpenRouter Responses request input field was not an array".to_string(),
            )
        })?;

    for item in input {
        let Some(message) = item.as_object_mut() else {
            continue;
        };
        if message.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }

        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        let Some(text) = content.as_str() else {
            continue;
        };

        let block_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        *content = json!([{ "type": block_type, "text": text }]);
    }

    Ok(())
}

async fn consume_sse_events(
    response: reqwest::Response,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> Result<CompletionResponse> {
    let mut state = OpenRouterStreamState::new(fallback_model);
    let byte_stream = response.bytes_stream().map_err(|error| {
        std::io::Error::other(format!("failed to read OpenRouter SSE bytes: {error}"))
    });
    let reader = StreamReader::new(byte_stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();
    let mut event_name = None::<String>;
    let mut data = String::new();

    loop {
        let line = lines.next_line().await.map_err(|error| {
            MoaError::StreamError(format!("failed to read OpenRouter SSE line: {error}"))
        })?;

        match line {
            Some(line) => {
                if line.is_empty() {
                    if data.is_empty() {
                        event_name = None;
                        continue;
                    }

                    let frame = OpenRouterSseFrame {
                        event: event_name.take(),
                        data: std::mem::take(&mut data),
                    };
                    let emitted = state.apply_event(&frame)?;

                    for block in emitted {
                        span_recorder.observe_block(&block);
                        if tx.send(Ok(block)).await.is_err() {
                            tracing::debug!(
                                "completion stream receiver dropped before the response finished"
                            );
                            return state.finish(started_at);
                        }
                    }
                    continue;
                }

                if line.starts_with(':') {
                    continue;
                }

                if let Some(event) = line.strip_prefix("event:") {
                    event_name = Some(event.strip_prefix(' ').unwrap_or(event).to_string());
                    continue;
                }

                if let Some(chunk) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(chunk.strip_prefix(' ').unwrap_or(chunk));
                }
            }
            None => {
                if !data.is_empty() {
                    let frame = OpenRouterSseFrame {
                        event: event_name.take(),
                        data,
                    };
                    let emitted = state.apply_event(&frame)?;

                    for block in emitted {
                        span_recorder.observe_block(&block);
                        if tx.send(Ok(block)).await.is_err() {
                            tracing::debug!(
                                "completion stream receiver dropped before the response finished"
                            );
                            return state.finish(started_at);
                        }
                    }
                }
                break;
            }
        }
    }

    state.finish(started_at)
}

#[derive(Debug)]
struct OpenRouterSseFrame {
    event: Option<String>,
    data: String,
}

#[derive(Debug)]
struct OpenRouterStreamState {
    model: String,
    response: Option<Response>,
    response_text: String,
    response_content: Vec<CompletionContent>,
    function_items: HashMap<String, FunctionToolCall>,
    emitted_function_items: HashSet<String>,
    search_started_emitted: bool,
    search_completed_emitted: bool,
}

impl OpenRouterStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            response: None,
            response_text: String::new(),
            response_content: Vec::new(),
            function_items: HashMap::new(),
            emitted_function_items: HashSet::new(),
            search_started_emitted: false,
            search_completed_emitted: false,
        }
    }

    fn apply_event(&mut self, event: &OpenRouterSseFrame) -> Result<Vec<CompletionContent>> {
        let payload = event.data.trim();
        if payload.is_empty()
            || payload == "[DONE]"
            || (!payload.starts_with('{') && !payload.starts_with('['))
        {
            return Ok(Vec::new());
        }

        let value: Value = serde_json::from_str(payload).map_err(|error| {
            MoaError::SerializationError(format!(
                "failed to parse OpenRouter SSE payload for event '{}': {error}",
                event.event.as_deref().unwrap_or("message")
            ))
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let mut emitted = match event_type {
            "response.output_text.delta" => self.apply_text_delta(&value)?,
            "response.output_item.added" | "response.output_item.done" => {
                self.apply_output_item_event(&value)?
            }
            "response.function_call_arguments.done" => {
                self.apply_function_arguments_done(&value)?
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.apply_response_event(&value)?
            }
            "response.error" | "error" => {
                return Err(MoaError::ProviderError(error_message(&value)));
            }
            _ => Vec::new(),
        };

        emitted.extend(self.summarize_search_event(&value, event_type, event.event.as_deref()));
        Ok(emitted)
    }

    fn apply_text_delta(&mut self, value: &Value) -> Result<Vec<CompletionContent>> {
        let delta = value.get("delta").and_then(Value::as_str).ok_or_else(|| {
            MoaError::SerializationError(
                "OpenRouter text delta event did not include a string delta".to_string(),
            )
        })?;
        if delta.is_empty() {
            return Ok(Vec::new());
        }

        self.response_text.push_str(delta);
        Ok(vec![CompletionContent::Text(delta.to_string())])
    }

    fn apply_output_item_event(&mut self, value: &Value) -> Result<Vec<CompletionContent>> {
        let item = value.get("item").cloned().unwrap_or(Value::Null);
        if let Ok(OutputItem::FunctionCall(call)) = serde_json::from_value(item)
            && let Some(item_id) = call.id.clone()
        {
            self.function_items.insert(item_id, call);
        }
        Ok(Vec::new())
    }

    fn apply_function_arguments_done(&mut self, value: &Value) -> Result<Vec<CompletionContent>> {
        let item_id = value
            .get("item_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MoaError::SerializationError(
                    "OpenRouter function arguments event did not include an item_id".to_string(),
                )
            })?
            .to_string();

        if self.emitted_function_items.contains(&item_id) {
            return Ok(Vec::new());
        }

        let arguments = value
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MoaError::SerializationError(
                    "OpenRouter function arguments event did not include arguments".to_string(),
                )
            })?;
        let input = parse_tool_arguments(arguments)?;
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                self.function_items
                    .get(&item_id)
                    .map(|call| call.name.clone())
            })
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "response function call {item_id} did not include a tool name"
                ))
            })?;

        let call = CompletionContent::ToolCall(moa_core::ToolInvocation {
            id: Some(item_id.clone()),
            name,
            input,
        });
        self.emitted_function_items.insert(item_id);
        self.response_content.push(call.clone());
        Ok(vec![call])
    }

    fn apply_response_event(&mut self, value: &Value) -> Result<Vec<CompletionContent>> {
        let response_value = value.get("response").cloned().ok_or_else(|| {
            MoaError::SerializationError(
                "OpenRouter response lifecycle event did not include a response object".to_string(),
            )
        })?;
        let response =
            serde_json::from_value::<Response>(response_value.clone()).map_err(|error| {
                MoaError::SerializationError(format!(
                    "failed to decode OpenRouter response object: {error}"
                ))
            })?;

        let web_search_requests = response_value
            .get("usage")
            .and_then(|usage| usage.get("server_tool_use"))
            .and_then(|usage| usage.get("web_search_requests"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        self.model = response.model.clone();
        self.response = Some(response);

        if web_search_requests > 0 && !self.search_completed_emitted {
            self.search_completed_emitted = true;
            return Ok(vec![CompletionContent::ProviderToolResult {
                tool_name: "web_search".to_string(),
                summary: format!("Web search completed ({} request(s)).", web_search_requests),
            }]);
        }

        Ok(Vec::new())
    }

    fn summarize_search_event(
        &mut self,
        value: &Value,
        event_type: &str,
        event_name: Option<&str>,
    ) -> Vec<CompletionContent> {
        let serialized = value.to_string();
        let mentions_search = event_type.contains("web_search")
            || event_name.unwrap_or_default().contains("web_search")
            || serialized.contains("openrouter:web_search")
            || serialized.contains("\"web_search\"")
            || serialized.contains("\"web_search_call\"");

        if !mentions_search {
            return Vec::new();
        }

        if !self.search_started_emitted {
            self.search_started_emitted = true;
            return vec![CompletionContent::ProviderToolResult {
                tool_name: "web_search".to_string(),
                summary: "Searching the web...".to_string(),
            }];
        }

        if !self.search_completed_emitted
            && (event_type.contains("completed") || serialized.contains("\"completed\""))
        {
            self.search_completed_emitted = true;
            return vec![CompletionContent::ProviderToolResult {
                tool_name: "web_search".to_string(),
                summary: "Web search completed.".to_string(),
            }];
        }

        Vec::new()
    }

    fn finish(self, started_at: Instant) -> Result<CompletionResponse> {
        let response = self.response.ok_or_else(|| {
            MoaError::ProviderError(
                "OpenRouter stream ended before the provider returned a completed response"
                    .to_string(),
            )
        })?;

        let mut completion = completion_response_from_response(response, self.model, started_at)?;
        if !self.response_text.is_empty() {
            completion.text = self.response_text;
        }
        if !self.response_content.is_empty() {
            completion.content = self.response_content;
        }
        if completion.content.is_empty() && !completion.text.is_empty() {
            completion
                .content
                .push(CompletionContent::Text(completion.text.clone()));
        }
        Ok(completion)
    }
}

fn completion_response_from_response(
    response: Response,
    fallback_model: String,
    started_at: Instant,
) -> Result<CompletionResponse> {
    let text = response_text_from_output(&response.output);
    let mut content = response_content_from_output(&response.output)?;
    if content.is_empty() && !text.is_empty() {
        content.push(CompletionContent::Text(text.clone()));
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

fn error_message(value: &Value) -> String {
    value
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("error")
                .and_then(Value::as_object)
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "OpenRouter stream returned an unknown error".to_string())
}

fn native_tools(
    capabilities: &ModelCapabilities,
    enabled: bool,
) -> &[moa_core::ProviderNativeTool] {
    if enabled {
        &capabilities.native_tools
    } else {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use moa_core::CompletionRequest;

    use super::{
        build_request_body, canonical_model_id, capabilities_for_model, native_web_search_tools,
    };

    #[test]
    fn normalizes_vendorless_models_to_provider_prefixed_routes() {
        assert_eq!(canonical_model_id("gpt-5.4"), "openai/gpt-5.4");
        assert_eq!(
            canonical_model_id("claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4-6"
        );
        assert_eq!(
            canonical_model_id("openai/gpt-5.4"),
            "openai/gpt-5.4".to_string()
        );
    }

    #[test]
    fn capability_lookup_reuses_known_model_families() {
        let openai_capabilities = capabilities_for_model("openai/gpt-5.4");
        assert_eq!(openai_capabilities.context_window, 1_050_000);
        assert!(openai_capabilities.supports_prefix_caching);
        assert_eq!(openai_capabilities.native_tools.len(), 1);

        let anthropic_capabilities = capabilities_for_model("anthropic/claude-opus-4-6");
        assert_eq!(anthropic_capabilities.context_window, 1_000_000);
        assert_eq!(anthropic_capabilities.max_output, 128_000);
        assert_eq!(anthropic_capabilities.native_tools.len(), 1);
    }

    #[test]
    fn serializes_responses_body_with_openrouter_server_tool_shape() {
        let body = build_request_body(
            &CompletionRequest::simple("What happened in the news today?"),
            "openai/gpt-4o-mini",
            "medium",
            &native_web_search_tools(),
        )
        .expect("request body");

        let input = body
            .get("input")
            .and_then(serde_json::Value::as_array)
            .expect("input array");
        let message = input.first().expect("first message");
        assert_eq!(
            message.get("type").and_then(serde_json::Value::as_str),
            Some("message")
        );
        assert_eq!(
            message.get("role").and_then(serde_json::Value::as_str),
            Some("user")
        );
        let content = message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .and_then(|content| content.first())
            .expect("input text block");
        assert_eq!(
            content.get("type").and_then(serde_json::Value::as_str),
            Some("input_text")
        );
        assert_eq!(
            content.get("text").and_then(serde_json::Value::as_str),
            Some("What happened in the news today?")
        );

        let tool = body
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .and_then(|tools| tools.first())
            .expect("native tool");
        assert_eq!(
            tool.get("type").and_then(serde_json::Value::as_str),
            Some("openrouter:web_search")
        );
        assert_eq!(
            tool.pointer("/parameters/engine")
                .and_then(serde_json::Value::as_str),
            Some("auto")
        );
    }
}
