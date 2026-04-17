//! Anthropic Claude provider implementation with SSE streaming support.
//!
//! Internal adapter phases:
//! 1. build one Anthropic Messages request body
//! 2. execute provider transport with shared retry handling
//! 3. normalize SSE events into `CompletionContent`
//! 4. finalize one normalized `CompletionResponse`
//! 5. record provider-private stream snapshots for tracing/debugging

use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_util::{Stream, StreamExt, pin_mut};
use moa_core::{
    CacheBreakpoint, CacheBreakpointTarget, CacheTtl, CompletionContent, CompletionRequest,
    CompletionResponse, CompletionStream, ContextMessage, LLMProvider, MessageRole, MoaConfig,
    MoaError, ModelCapabilities, ModelId, ProviderNativeTool, Result, StopReason, TokenPricing,
    TokenUsage, ToolCallFormat, ToolContent, ToolInvocation, estimate_text_tokens,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::http::build_http_client;
use crate::instrumentation::LLMSpanRecorder;
use crate::retry::RetryPolicy;
use crate::sse::parse_sse_json;

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_STREAM_BUFFER: usize = 64;
const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 4_096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const MIN_CACHEABLE_TOKENS: usize = 1_024;
const MODEL_OPUS_4_6: &str = "claude-opus-4-6";
const MODEL_SONNET_4_6: &str = "claude-sonnet-4-6";

#[derive(Debug, Clone, Copy)]
enum CacheTarget {
    System(usize),
    Message(usize),
}

/// Anthropic Claude provider backed by the Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: Arc<str>,
    default_model: String,
    default_capabilities: ModelCapabilities,
    messages_url: Arc<str>,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
}

impl AnthropicProvider {
    /// Creates a provider from an API key and default model identifier.
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Result<Self> {
        let default_model = default_model.into();
        let resolved_default_model = canonical_model_id(&default_model)?;
        let default_capabilities = capabilities_for_model(&resolved_default_model)?;

        Ok(Self {
            client: build_http_client()?,
            api_key: Arc::from(api_key.into()),
            default_model: resolved_default_model,
            default_capabilities,
            messages_url: Arc::from(ANTHROPIC_MESSAGES_URL),
            retry_policy: RetryPolicy::default().with_max_retries(DEFAULT_MAX_RETRIES),
            web_search_enabled: true,
        })
    }

    /// Creates a provider from the configured Anthropic environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.general.default_model.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key_env = config.providers.anthropic.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;

        Self::new(api_key, default_model)
            .map(|provider| provider.with_web_search_enabled(config.general.web_search_enabled))
    }

    /// Creates a provider from the `ANTHROPIC_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("ANTHROPIC_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("ANTHROPIC_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the Messages API URL, primarily for tests.
    pub fn with_messages_url(mut self, messages_url: impl Into<String>) -> Self {
        self.messages_url = Arc::from(messages_url.into());
        self
    }

    /// Overrides the retry budget for rate-limited requests.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.retry_policy = self.retry_policy.with_max_retries(max_retries);
        self
    }

    /// Overrides whether provider-native web search is exposed to supported models.
    pub fn with_web_search_enabled(mut self, enabled: bool) -> Self {
        self.web_search_enabled = enabled;
        self
    }
}

#[async_trait::async_trait]
impl LLMProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.default_capabilities.clone()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let requested_model = request
            .model
            .as_ref()
            .map(ModelId::as_str)
            .unwrap_or(self.default_model.as_str())
            .to_string();
        let resolved_model = canonical_model_id(&requested_model)?;
        let model_capabilities = capabilities_for_model(&resolved_model)?;
        let max_output_tokens = Some(
            request
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
                .min(model_capabilities.max_output),
        );
        let span_recorder = LLMSpanRecorder::new(
            "anthropic",
            resolved_model.clone(),
            &request,
            max_output_tokens,
            model_capabilities.pricing.clone(),
        );
        span_recorder.set_phase("build_request");
        let span = span_recorder.span().clone();
        let request_body = match build_request_body(
            &request,
            &resolved_model,
            &model_capabilities,
            self.web_search_enabled,
        ) {
            Ok(body) => body,
            Err(error) => {
                span_recorder.fail_at_stage("build_request", &error);
                return Err(error);
            }
        };
        let client = self.client.clone();
        let api_key = Arc::clone(&self.api_key);
        let messages_url = Arc::clone(&self.messages_url);
        let retry_policy = self.retry_policy.clone();
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                let mut span_recorder = span_recorder;
                let started_at = Instant::now();
                span_recorder.set_phase("transport");
                let response = retry_policy
                    .send(|| {
                        client
                            .post(&*messages_url)
                            .header("x-api-key", &*api_key)
                            .header("anthropic-version", ANTHROPIC_API_VERSION)
                            .header(ACCEPT, "text/event-stream")
                            .header(CONTENT_TYPE, "application/json")
                            .json(&request_body)
                    })
                    .await;

                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        span_recorder.fail_at_stage("transport", &error);
                        return Err(error);
                    }
                };

                span_recorder.set_phase("stream");
                let response = consume_sse_events(
                    response.bytes_stream().eventsource(),
                    tx,
                    resolved_model,
                    started_at,
                    &mut span_recorder,
                )
                .await;

                match response {
                    Ok(response) => {
                        span_recorder.set_phase("finalize");
                        span_recorder.finish(&response);
                        Ok(response)
                    }
                    Err(error) => {
                        span_recorder.fail_at_stage("stream", &error);
                        Err(error)
                    }
                }
            }
            .instrument(span),
        );

        Ok(CompletionStream::new(rx, completion_task))
    }
}

fn canonical_model_id(model: &str) -> Result<String> {
    match model {
        MODEL_OPUS_4_6 => Ok(MODEL_OPUS_4_6.to_string()),
        MODEL_SONNET_4_6 => Ok(MODEL_SONNET_4_6.to_string()),
        unsupported => Err(MoaError::Unsupported(format!(
            "unsupported Anthropic model '{unsupported}'"
        ))),
    }
}

fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    match model {
        MODEL_OPUS_4_6 => Ok(ModelCapabilities {
            model_id: ModelId::new(MODEL_OPUS_4_6),
            context_window: 1_000_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 5.0,
                output_per_mtok: 25.0,
                cached_input_per_mtok: Some(0.5),
            },
            native_tools: native_web_search_tools(),
        }),
        MODEL_SONNET_4_6 => Ok(ModelCapabilities {
            model_id: ModelId::new(MODEL_SONNET_4_6),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: native_web_search_tools(),
        }),
        unsupported => Err(MoaError::Unsupported(format!(
            "unsupported Anthropic model '{unsupported}'"
        ))),
    }
}

fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    capabilities: &ModelCapabilities,
    web_search_enabled: bool,
) -> Result<Value> {
    let mut system_messages = Vec::new();
    let mut messages = Vec::new();
    let mut cache_targets = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        if message.role == MessageRole::System {
            let system_index = system_messages.len();
            system_messages.push(anthropic_text_block(message.content.clone()));
            cache_targets.push(CacheTarget::System(system_index));
            continue;
        }

        let message_index = messages.len();
        messages.push(anthropic_message(message)?);
        cache_targets.push(CacheTarget::Message(message_index));
    }

    if messages.is_empty() {
        return Err(MoaError::ValidationError(
            "Anthropic requests require at least one non-system message".to_string(),
        ));
    }

    let max_tokens = request
        .max_output_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        .min(capabilities.max_output);

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(model.to_string()));
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), Value::Bool(true));

    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), json!(temperature));
    }

    let mut tools = request
        .tools
        .iter()
        .map(anthropic_tool_from_schema)
        .collect::<Vec<_>>();
    if web_search_enabled {
        tools.extend(
            capabilities
                .native_tools
                .iter()
                .map(provider_native_tool_json),
        );
    }

    apply_cache_breakpoints(
        request,
        &cache_targets,
        &mut system_messages,
        &mut messages,
        &mut tools,
    );

    body.insert("messages".to_string(), Value::Array(messages));
    if !system_messages.is_empty() {
        body.insert("system".to_string(), Value::Array(system_messages));
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    Ok(Value::Object(body))
}

#[cfg(any(test, feature = "test-util"))]
/// Builds the Anthropic request body for inspection in integration tests without sending it.
pub fn debug_build_request_body(
    request: &CompletionRequest,
    web_search_enabled: bool,
) -> Result<Value> {
    let requested_model = request
        .model
        .as_ref()
        .map(ModelId::as_str)
        .unwrap_or(MODEL_SONNET_4_6);
    let resolved_model = canonical_model_id(requested_model)?;
    let capabilities = capabilities_for_model(&resolved_model)?;
    build_request_body(request, &resolved_model, &capabilities, web_search_enabled)
}

fn anthropic_text_block(text: impl Into<String>) -> Value {
    json!({
        "type": "text",
        "text": text.into(),
    })
}

fn apply_cache_breakpoints(
    request: &CompletionRequest,
    cache_targets: &[CacheTarget],
    system_messages: &mut [Value],
    messages: &mut [Value],
    tools: &mut [Value],
) {
    for breakpoint in eligible_cache_breakpoints(request, MAX_CACHE_BREAKPOINTS) {
        match breakpoint.target {
            CacheBreakpointTarget::ToolDefinitions => {
                if let Some(last_tool) = tools.last_mut() {
                    annotate_cache_control(last_tool, breakpoint.ttl);
                }
            }
            CacheBreakpointTarget::MessageBoundary { index } => {
                let Some(target_index) = index.checked_sub(1) else {
                    continue;
                };
                let Some(target) = cache_targets.get(target_index).copied() else {
                    continue;
                };

                match target {
                    CacheTarget::System(index) => {
                        if let Some(block) = system_messages.get_mut(index) {
                            annotate_cache_control(block, breakpoint.ttl);
                        }
                    }
                    CacheTarget::Message(index) => {
                        if let Some(message) = messages.get_mut(index) {
                            annotate_message_cache_control(message, breakpoint.ttl);
                        }
                    }
                }
            }
        }
    }
}

fn eligible_cache_breakpoints(
    request: &CompletionRequest,
    max_breakpoints: usize,
) -> Vec<CacheBreakpoint> {
    let mut requested = requested_cache_breakpoints(request);
    if requested.is_empty() || max_breakpoints == 0 {
        return Vec::new();
    }

    requested.sort_by_key(cache_breakpoint_sort_key);
    requested.dedup();

    let mut eligible = Vec::new();
    let tool_tokens = request
        .tools
        .iter()
        .map(|tool| estimate_text_tokens(&tool.to_string()))
        .sum::<usize>();
    let mut prefix_tokens = tool_tokens;
    let mut next_breakpoint = 0usize;

    while let Some(breakpoint) = requested.get(next_breakpoint).cloned() {
        match breakpoint.target {
            CacheBreakpointTarget::ToolDefinitions => {
                if tool_tokens >= MIN_CACHEABLE_TOKENS {
                    eligible.push(breakpoint);
                }
                next_breakpoint += 1;
            }
            CacheBreakpointTarget::MessageBoundary { .. } => break,
        }
    }

    for (index, message) in request.messages.iter().enumerate() {
        prefix_tokens += estimate_text_tokens(&message.content);

        while let Some(breakpoint) = requested.get(next_breakpoint).cloned() {
            let CacheBreakpointTarget::MessageBoundary {
                index: breakpoint_index,
            } = breakpoint.target
            else {
                break;
            };
            if breakpoint_index != index + 1 {
                break;
            }
            if prefix_tokens >= MIN_CACHEABLE_TOKENS {
                eligible.push(breakpoint);
            }
            next_breakpoint += 1;
        }
    }

    eligible
        .into_iter()
        .rev()
        .take(max_breakpoints)
        .rev()
        .collect()
}

fn requested_cache_breakpoints(request: &CompletionRequest) -> Vec<CacheBreakpoint> {
    if !request.cache_controls.is_empty() {
        return request.cache_controls.clone();
    }

    request
        .cache_breakpoints
        .iter()
        .copied()
        .filter(|breakpoint| *breakpoint > 0)
        .map(|index| CacheBreakpoint::message(index, CacheTtl::OneHour))
        .collect()
}

fn cache_breakpoint_sort_key(breakpoint: &CacheBreakpoint) -> (usize, usize) {
    match breakpoint.target {
        CacheBreakpointTarget::ToolDefinitions => (0, 0),
        CacheBreakpointTarget::MessageBoundary { index } => (1, index),
    }
}

fn annotate_cache_control(value: &mut Value, ttl: CacheTtl) {
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "cache_control".to_string(),
            json!({
                "type": "ephemeral",
                "ttl": ttl.as_anthropic_ttl(),
            }),
        );
    }
}

fn annotate_message_cache_control(message: &mut Value, ttl: CacheTtl) {
    let Some(content) = message.get_mut("content") else {
        return;
    };

    match content {
        Value::String(text) => {
            let text = std::mem::take(text);
            *content = Value::Array(vec![anthropic_text_block(text)]);
            if let Some(blocks) = content.as_array_mut()
                && let Some(last_block) = blocks.last_mut()
            {
                annotate_cache_control(last_block, ttl);
            }
        }
        Value::Array(blocks) => {
            if let Some(last_block) = blocks.last_mut() {
                annotate_cache_control(last_block, ttl);
            }
        }
        _ => {}
    }
}

fn native_web_search_tools() -> Vec<ProviderNativeTool> {
    vec![ProviderNativeTool {
        tool_type: "web_search_20250305".to_string(),
        name: "web_search".to_string(),
        config: None,
    }]
}

fn provider_native_tool_json(tool: &ProviderNativeTool) -> Value {
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

fn summarize_anthropic_server_tool_use(name: &str, partial_json: &str) -> String {
    if name == "web_search"
        && let Ok(value) = serde_json::from_str::<Value>(partial_json)
        && let Some(query) = value.get("query").and_then(Value::as_str)
    {
        return format!("Searching the web for: {query}");
    }

    format!("Running provider tool: {name}")
}

fn summarize_anthropic_search_results(content: &[WebSearchResultContent]) -> String {
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

fn anthropic_message(message: &ContextMessage) -> Result<Value> {
    if let Some(invocation) = message.tool_invocation.as_ref() {
        return Ok(json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": invocation
                    .id
                    .clone()
                    .unwrap_or_else(|| "unknown_tool_use".to_string()),
                "name": invocation.name,
                "input": invocation.input,
            }]
        }));
    }

    if message.role == MessageRole::Tool {
        let content = if let Some(blocks) = &message.content_blocks {
            anthropic_content_blocks(blocks)
        } else {
            json!(message.content)
        };

        return Ok(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": message
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(|| "unknown_tool_use".to_string()),
                "content": content,
            }]
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

fn anthropic_content_blocks(blocks: &[ToolContent]) -> Value {
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

fn anthropic_tool_from_schema(schema: &Value) -> Value {
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

async fn consume_sse_events<S, E>(
    events: S,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> Result<CompletionResponse>
where
    S: Stream<Item = std::result::Result<SseEvent, E>>,
    E: std::fmt::Display,
{
    let mut state = AnthropicStreamState::new(fallback_model);
    pin_mut!(events);

    while let Some(event) = events.next().await {
        let event = event
            .map_err(|error| MoaError::StreamError(format!("failed to read SSE event: {error}")))?;
        let emitted = state.apply_event(&event)?;

        for block in emitted {
            span_recorder.observe_block(block.clone());
            if tx.send(Ok(block)).await.is_err() {
                tracing::debug!("completion stream receiver dropped before the response finished");
                break;
            }
        }
    }

    span_recorder.record_raw_response(&state.debug_snapshot());
    span_recorder.set_cached_input_tokens(state.cached_input_tokens);
    span_recorder.set_cache_creation_input_tokens(state.cache_creation_input_tokens);
    Ok(state.finish(started_at))
}

#[derive(Debug, Serialize)]
struct AnthropicStreamDebugSnapshot {
    model: String,
    stop_reason: StopReason,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
    content: Vec<CompletionContent>,
}

#[derive(Debug)]
struct AnthropicStreamState {
    model: String,
    stop_reason: StopReason,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    cache_creation_input_tokens: usize,
    blocks: Vec<BlockAccumulator>,
    completed_content: Vec<Option<CompletionContent>>,
}

impl AnthropicStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            stop_reason: StopReason::Other("unknown".to_string()),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            blocks: Vec::new(),
            completed_content: Vec::new(),
        }
    }

    fn apply_event(&mut self, event: &SseEvent) -> Result<Vec<CompletionContent>> {
        match event.event.as_str() {
            "message_start" => {
                let payload: MessageStartEvent = parse_sse_json(event)?;
                self.model = payload.message.model;
                if let Some(usage) = payload.message.usage {
                    self.input_tokens = usage.input_tokens;
                    self.cached_input_tokens = usage.cache_read_input_tokens;
                    self.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                }
                Ok(Vec::new())
            }
            "content_block_start" => self.apply_block_start(parse_sse_json(event)?),
            "content_block_delta" => self.apply_block_delta(parse_sse_json(event)?),
            "content_block_stop" => Ok(self.apply_block_stop(parse_sse_json(event)?)),
            "message_delta" => {
                let payload: MessageDeltaEvent = parse_sse_json(event)?;
                self.stop_reason = payload
                    .delta
                    .stop_reason
                    .map(stop_reason_from_anthropic)
                    .unwrap_or_else(|| StopReason::Other("unknown".to_string()));
                if let Some(usage) = payload.usage {
                    self.output_tokens = usage.output_tokens;
                    if usage.cache_read_input_tokens > 0 {
                        self.cached_input_tokens = usage.cache_read_input_tokens;
                    }
                    if usage.cache_creation_input_tokens > 0 {
                        self.cache_creation_input_tokens = usage.cache_creation_input_tokens;
                    }
                }
                Ok(Vec::new())
            }
            "message_stop" | "ping" => Ok(Vec::new()),
            "error" => {
                let payload: ErrorEvent = parse_sse_json(event)?;
                Err(MoaError::ProviderError(format!(
                    "Anthropic stream error ({}): {}",
                    payload.error.kind, payload.error.message
                )))
            }
            _ => {
                tracing::debug!(event = %event.event, "ignoring unknown Anthropic SSE event");
                Ok(Vec::new())
            }
        }
    }

    fn apply_block_start(
        &mut self,
        payload: ContentBlockStartEvent,
    ) -> Result<Vec<CompletionContent>> {
        self.ensure_capacity(payload.index);
        match payload.content_block {
            ContentBlockStart::Text { text } => {
                self.blocks[payload.index] = BlockAccumulator::Text(text.clone());
                if text.is_empty() {
                    Ok(Vec::new())
                } else {
                    Ok(vec![CompletionContent::Text(text)])
                }
            }
            ContentBlockStart::ToolUse { id, name, input } => {
                let partial_json = initial_tool_input(input)?;
                self.blocks[payload.index] = BlockAccumulator::Tool {
                    id,
                    name,
                    partial_json,
                };
                Ok(Vec::new())
            }
            ContentBlockStart::ServerToolUse { _id: _, name } => {
                self.blocks[payload.index] = BlockAccumulator::ServerTool {
                    name,
                    partial_json: String::new(),
                };
                Ok(Vec::new())
            }
            ContentBlockStart::WebSearchToolResult {
                _tool_use_id: _,
                content,
            } => {
                self.blocks[payload.index] = BlockAccumulator::Ignored;
                self.ensure_completed_capacity(payload.index);
                let block = CompletionContent::ProviderToolResult {
                    tool_name: "web_search".to_string(),
                    summary: summarize_anthropic_search_results(&content),
                };
                self.completed_content[payload.index] = Some(block.clone());
                Ok(vec![block])
            }
            ContentBlockStart::Unknown => {
                self.blocks[payload.index] = BlockAccumulator::Ignored;
                Ok(Vec::new())
            }
        }
    }

    fn apply_block_delta(
        &mut self,
        payload: ContentBlockDeltaEvent,
    ) -> Result<Vec<CompletionContent>> {
        self.ensure_capacity(payload.index);
        match (&mut self.blocks[payload.index], payload.delta) {
            (BlockAccumulator::Text(text), ContentBlockDelta::TextDelta { text: delta }) => {
                text.push_str(&delta);
                Ok(vec![CompletionContent::Text(delta)])
            }
            (
                BlockAccumulator::Tool { partial_json, .. },
                ContentBlockDelta::InputJsonDelta {
                    partial_json: delta,
                },
            ) => {
                partial_json.push_str(&delta);
                Ok(Vec::new())
            }
            (
                BlockAccumulator::ServerTool { partial_json, .. },
                ContentBlockDelta::InputJsonDelta {
                    partial_json: delta,
                },
            ) => {
                partial_json.push_str(&delta);
                Ok(Vec::new())
            }
            (_, ContentBlockDelta::Unknown) => Ok(Vec::new()),
            _ => Err(MoaError::StreamError(
                "received an Anthropic content delta that did not match the active block"
                    .to_string(),
            )),
        }
    }

    fn apply_block_stop(&mut self, payload: ContentBlockStopEvent) -> Vec<CompletionContent> {
        self.ensure_capacity(payload.index);
        self.ensure_completed_capacity(payload.index);

        let block = std::mem::replace(&mut self.blocks[payload.index], BlockAccumulator::Ignored);
        match block {
            BlockAccumulator::Text(text) => {
                self.completed_content[payload.index] = Some(CompletionContent::Text(text));
                Vec::new()
            }
            BlockAccumulator::Tool {
                id,
                name,
                partial_json,
            } => {
                let input = if partial_json.trim().is_empty() {
                    Value::Object(Map::new())
                } else {
                    match serde_json::from_str(&partial_json) {
                        Ok(value) => value,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                tool_name = %name,
                                payload_bytes = partial_json.len(),
                                "Anthropic tool input JSON failed to parse; falling back to empty object"
                            );
                            Value::Object(Map::new())
                        }
                    }
                };
                let tool_call = ToolInvocation {
                    id: Some(id),
                    name,
                    input,
                };
                let content = CompletionContent::ToolCall(moa_core::ToolCallContent {
                    invocation: tool_call.clone(),
                    provider_metadata: None,
                });
                self.completed_content[payload.index] = Some(content.clone());
                vec![content]
            }
            BlockAccumulator::ServerTool { name, partial_json } => {
                let block = CompletionContent::ProviderToolResult {
                    tool_name: name.clone(),
                    summary: summarize_anthropic_server_tool_use(&name, &partial_json),
                };
                self.completed_content[payload.index] = Some(block.clone());
                vec![block]
            }
            BlockAccumulator::Ignored => Vec::new(),
        }
    }

    fn finish(mut self, started_at: Instant) -> CompletionResponse {
        for index in 0..self.blocks.len() {
            self.ensure_completed_capacity(index);
            match &self.blocks[index] {
                BlockAccumulator::Text(text) => {
                    if self.completed_content[index].is_none() {
                        self.completed_content[index] = Some(CompletionContent::Text(text.clone()));
                    }
                }
                BlockAccumulator::Tool {
                    id,
                    name,
                    partial_json,
                } => {
                    if self.completed_content[index].is_none() {
                        let input = if partial_json.trim().is_empty() {
                            Value::Object(Map::new())
                        } else {
                            match serde_json::from_str(partial_json) {
                                Ok(value) => value,
                                Err(error) => {
                                    tracing::warn!(
                                        %error,
                                        tool_name = %name,
                                        payload_bytes = partial_json.len(),
                                        "Anthropic tool input JSON failed to parse on finish; falling back to empty object"
                                    );
                                    Value::Object(Map::new())
                                }
                            }
                        };
                        self.completed_content[index] =
                            Some(CompletionContent::ToolCall(moa_core::ToolCallContent {
                                invocation: ToolInvocation {
                                    id: Some(id.clone()),
                                    name: name.clone(),
                                    input,
                                },
                                provider_metadata: None,
                            }));
                    }
                }
                BlockAccumulator::ServerTool { name, partial_json } => {
                    if self.completed_content[index].is_none() {
                        self.completed_content[index] =
                            Some(CompletionContent::ProviderToolResult {
                                tool_name: name.clone(),
                                summary: summarize_anthropic_server_tool_use(name, partial_json),
                            });
                    }
                }
                BlockAccumulator::Ignored => {}
            }
        }

        let content: Vec<_> = self.completed_content.into_iter().flatten().collect();
        let text = content
            .iter()
            .filter_map(|block| match block {
                CompletionContent::Text(text) => Some(text.as_str()),
                CompletionContent::ToolCall(_) => None,
                CompletionContent::ProviderToolResult { .. } => None,
            })
            .collect::<String>();

        CompletionResponse {
            text,
            content,
            stop_reason: self.stop_reason,
            model: ModelId::new(self.model),
            input_tokens: self.input_tokens
                + self.cached_input_tokens
                + self.cache_creation_input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            usage: TokenUsage {
                input_tokens_uncached: self.input_tokens,
                input_tokens_cache_write: self.cache_creation_input_tokens,
                input_tokens_cache_read: self.cached_input_tokens,
                output_tokens: self.output_tokens,
            },
            duration_ms: started_at.elapsed().as_millis() as u64,
            thought_signature: None,
        }
    }

    fn debug_snapshot(&self) -> AnthropicStreamDebugSnapshot {
        AnthropicStreamDebugSnapshot {
            model: self.model.clone(),
            stop_reason: self.stop_reason.clone(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            content: self.completed_content.iter().flatten().cloned().collect(),
        }
    }

    fn ensure_capacity(&mut self, index: usize) {
        while self.blocks.len() <= index {
            self.blocks.push(BlockAccumulator::Ignored);
        }
    }

    fn ensure_completed_capacity(&mut self, index: usize) {
        while self.completed_content.len() <= index {
            self.completed_content.push(None);
        }
    }
}

#[derive(Debug, Clone)]
enum BlockAccumulator {
    Text(String),
    Tool {
        id: String,
        name: String,
        partial_json: String,
    },
    ServerTool {
        name: String,
        partial_json: String,
    },
    Ignored,
}

#[derive(Debug, Deserialize)]
struct MessageStartEvent {
    message: MessageEnvelope,
}

#[derive(Debug, Deserialize)]
struct MessageEnvelope {
    model: String,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    #[serde(default)]
    cache_read_input_tokens: usize,
    #[serde(default)]
    cache_creation_input_tokens: usize,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStartEvent {
    index: usize,
    content_block: ContentBlockStart,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockStart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: Value,
    },
    ServerToolUse {
        #[serde(rename = "id")]
        _id: String,
        name: String,
    },
    WebSearchToolResult {
        #[serde(rename = "tool_use_id")]
        _tool_use_id: String,
        #[serde(default)]
        content: Vec<WebSearchResultContent>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEvent {
    index: usize,
    delta: ContentBlockDelta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockDelta {
    TextDelta {
        text: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ContentBlockStopEvent {
    index: usize,
}

#[derive(Debug, Deserialize)]
struct WebSearchResultContent {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaEvent {
    delta: MessageDeltaPayload,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct MessageDeltaPayload {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorEvent {
    error: StreamErrorPayload,
}

#[derive(Debug, Deserialize)]
struct StreamErrorPayload {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

fn initial_tool_input(input: Value) -> Result<String> {
    if input.is_null() {
        return Ok(String::new());
    }

    if let Value::Object(map) = &input
        && map.is_empty()
    {
        return Ok(String::new());
    }

    serde_json::to_string(&input).map_err(MoaError::from)
}

fn stop_reason_from_anthropic(stop_reason: String) -> StopReason {
    match stop_reason.as_str() {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "cancelled" => StopReason::Cancelled,
        other => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use eventsource_stream::Eventsource;
    use futures_util::stream;
    use moa_core::{
        CacheBreakpoint, CacheTtl, CompletionContent, CompletionRequest, ContextMessage,
        LLMProvider, ModelId, StopReason, ToolContent,
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{
        AnthropicProvider, MODEL_OPUS_4_6, MODEL_SONNET_4_6, anthropic_content_blocks,
        anthropic_message, anthropic_tool_from_schema, build_request_body, canonical_model_id,
        capabilities_for_model, consume_sse_events,
    };
    use crate::instrumentation::LLMSpanRecorder;

    #[test]
    fn completion_request_serializes_to_anthropic_format() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::system("System one"),
                ContextMessage::system("System two"),
                ContextMessage::user("Hello"),
                ContextMessage::assistant("Hi"),
            ],
            tools: vec![json!({
                "name": "bash",
                "description": "Run shell commands",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" }
                    },
                    "required": ["cmd"]
                }
            })],
            max_output_tokens: Some(512),
            temperature: Some(0.2),
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
            &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
            true,
        )
        .unwrap();

        assert_eq!(body["model"], MODEL_SONNET_4_6);
        assert_eq!(body["system"][0]["text"], "System one");
        assert_eq!(body["system"][1]["text"], "System two");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["input_schema"]["required"], json!(["cmd"]));
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn completion_request_prefers_deepest_breakpoint_when_static_prefix_fits_window() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::system("S".repeat(5_000)),
                ContextMessage::user("Hello"),
            ],
            tools: vec![json!({
                "name": "bash",
                "description": "Run shell commands",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" }
                    },
                    "required": ["cmd"]
                }
            })],
            max_output_tokens: Some(512),
            temperature: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::OneHour)],
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
            &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
            false,
        )
        .expect("request should build");

        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert!(body["tools"][0].get("cache_control").is_none());
    }

    #[test]
    fn completion_request_applies_cache_control_to_message_breakpoints() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::user("U".repeat(5_000)),
                ContextMessage::assistant("ack"),
            ],
            tools: Vec::new(),
            max_output_tokens: Some(512),
            temperature: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![CacheBreakpoint::message(1, CacheTtl::FiveMinutes)],
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
            &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
            false,
        )
        .expect("request should build");

        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "5m"
        );
    }

    #[test]
    fn completion_request_limits_cache_control_markers_to_four_total() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::user("A".repeat(5_000)),
                ContextMessage::user("B".repeat(5_000)),
                ContextMessage::user("C".repeat(5_000)),
                ContextMessage::user("D".repeat(5_000)),
                ContextMessage::user("E".repeat(5_000)),
                ContextMessage::assistant("done"),
            ],
            tools: vec![json!({
                "name": "bash",
                "description": "Run shell commands",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "cmd": { "type": "string" }
                    },
                    "required": ["cmd"]
                }
            })],
            max_output_tokens: Some(512),
            temperature: None,
            cache_breakpoints: vec![1, 2, 3, 4, 5],
            cache_controls: vec![
                CacheBreakpoint::tools(CacheTtl::OneHour),
                CacheBreakpoint::message(1, CacheTtl::OneHour),
                CacheBreakpoint::message(3, CacheTtl::OneHour),
                CacheBreakpoint::message(5, CacheTtl::FiveMinutes),
                CacheBreakpoint::message(6, CacheTtl::FiveMinutes),
            ],
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
            &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
            false,
        )
        .expect("request should build");

        let message_markers = body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter(|message| message["content"][0].get("cache_control").is_some())
            .count();
        let tool_markers = body["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter(|tool| tool.get("cache_control").is_some())
            .count();

        assert_eq!(message_markers + tool_markers, 4);
    }

    #[test]
    fn completion_request_counts_tool_tokens_toward_breakpoint_eligibility() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::system("brief"),
                ContextMessage::user("Hello"),
            ],
            tools: vec![json!({
                "name": "tool_a",
                "description": "A".repeat(6_000),
                "input_schema": { "type": "object" }
            })],
            max_output_tokens: Some(512),
            temperature: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![
                CacheBreakpoint::tools(CacheTtl::OneHour),
                CacheBreakpoint::message(1, CacheTtl::OneHour),
            ],
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
            &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
            false,
        )
        .expect("request should build");

        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn completion_request_marks_explicit_tool_breakpoint() {
        let request = CompletionRequest {
            model: Some(ModelId::new(MODEL_SONNET_4_6)),
            messages: vec![
                ContextMessage::system("S".repeat(5_000)),
                ContextMessage::user("Hello"),
            ],
            tools: (0..25)
                .map(|index| {
                    json!({
                        "name": format!("tool_{index}"),
                        "description": "Run shell commands ".repeat(80),
                        "input_schema": { "type": "object" }
                    })
                })
                .collect(),
            max_output_tokens: Some(512),
            temperature: None,
            cache_breakpoints: vec![1],
            cache_controls: vec![
                CacheBreakpoint::tools(CacheTtl::OneHour),
                CacheBreakpoint::message(1, CacheTtl::OneHour),
            ],
            metadata: Default::default(),
        };

        let body = build_request_body(
            &request,
            &canonical_model_id(MODEL_SONNET_4_6).expect("valid model"),
            &capabilities_for_model(MODEL_SONNET_4_6).expect("valid capabilities"),
            false,
        )
        .expect("request should build");

        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(
            tools
                .last()
                .and_then(|tool| tool.get("cache_control"))
                .and_then(|value| value.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("ephemeral")
        );
        assert_eq!(
            tools
                .last()
                .and_then(|tool| tool.get("cache_control"))
                .and_then(|value| value.get("ttl"))
                .and_then(serde_json::Value::as_str),
            Some("1h")
        );
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn completion_request_includes_native_web_search_when_enabled() {
        let body = build_request_body(
            &CompletionRequest::simple("What happened in the news today?"),
            &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
            &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
            true,
        )
        .unwrap();

        let tools = body["tools"].as_array().unwrap();
        assert!(
            tools
                .iter()
                .any(|tool| tool["type"] == "web_search_20250305" && tool["name"] == "web_search")
        );
    }

    #[test]
    fn completion_request_omits_native_web_search_when_disabled() {
        let body = build_request_body(
            &CompletionRequest::simple("What happened in the news today?"),
            &canonical_model_id(MODEL_SONNET_4_6).unwrap(),
            &capabilities_for_model(MODEL_SONNET_4_6).unwrap(),
            false,
        )
        .unwrap();

        assert!(body.get("tools").is_none());
    }

    #[test]
    fn anthropic_content_blocks_render_text_and_json_as_text_blocks() {
        let blocks = anthropic_content_blocks(&[
            ToolContent::Text {
                text: "summary".to_string(),
            },
            ToolContent::Json {
                data: json!({"path": "notes/today.md"}),
            },
        ]);

        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "<untrusted_tool_output>");
        assert_eq!(blocks[1]["text"], "summary");
        assert_eq!(blocks[2]["text"], "{\"path\":\"notes/today.md\"}");
        assert_eq!(blocks[3]["text"], "</untrusted_tool_output>");
    }

    #[test]
    fn anthropic_message_wraps_tool_results_with_tool_use_id() {
        let message = anthropic_message(&ContextMessage::tool_result(
            "toolu_123",
            "fallback",
            Some(vec![ToolContent::Text {
                text: "hello".to_string(),
            }]),
        ))
        .unwrap();

        assert_eq!(message["role"], "user");
        assert_eq!(message["content"][0]["type"], "tool_result");
        assert_eq!(message["content"][0]["tool_use_id"], "toolu_123");
        assert_eq!(message["content"][0]["content"][1]["text"], "hello");
    }

    #[test]
    fn anthropic_message_wraps_assistant_tool_calls_as_tool_use_blocks() {
        let message = anthropic_message(&ContextMessage::assistant_tool_call(
            moa_core::ToolInvocation {
                id: Some("toolu_234".to_string()),
                name: "file_write".to_string(),
                input: json!({ "path": "live/anthropic.txt" }),
            },
            "<tool_call name=\"file_write\">{\"path\":\"live/anthropic.txt\"}</tool_call>",
        ))
        .unwrap();

        assert_eq!(message["role"], "assistant");
        assert_eq!(message["content"][0]["type"], "tool_use");
        assert_eq!(message["content"][0]["id"], "toolu_234");
        assert_eq!(message["content"][0]["name"], "file_write");
        assert_eq!(message["content"][0]["input"]["path"], "live/anthropic.txt");
    }

    #[test]
    fn anthropic_tool_from_schema_moves_parameters_into_input_schema() {
        let tool = anthropic_tool_from_schema(&json!({
            "name": "memory_search",
            "description": "Search memory",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }
        }));

        assert_eq!(tool["name"], "memory_search");
        assert_eq!(tool["input_schema"]["required"], json!(["query"]));
    }

    #[tokio::test]
    async fn parses_recorded_sse_stream_into_content_blocks() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-6\",\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cmd\\\":\\\"ls\\\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let raw_stream = stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(sse.as_bytes().to_vec())]);
        let events = raw_stream.eventsource();
        let (tx, mut rx) = mpsc::channel(8);
        let request = CompletionRequest::new("Hello");
        let capabilities = capabilities_for_model(MODEL_SONNET_4_6).unwrap();
        let mut span_recorder = LLMSpanRecorder::new(
            "anthropic",
            MODEL_SONNET_4_6,
            &request,
            request.max_output_tokens,
            capabilities.pricing,
        );

        let response = consume_sse_events(
            events,
            tx,
            MODEL_SONNET_4_6.to_string(),
            Instant::now(),
            &mut span_recorder,
        )
        .await
        .unwrap();

        let mut streamed_blocks = Vec::new();
        while let Some(block) = rx.recv().await {
            streamed_blocks.push(block.unwrap());
        }

        assert_eq!(streamed_blocks.len(), 3);
        assert_eq!(
            streamed_blocks[0],
            CompletionContent::Text("Hel".to_string())
        );
        assert_eq!(
            streamed_blocks[1],
            CompletionContent::Text("lo".to_string())
        );
        match &streamed_blocks[2] {
            CompletionContent::ToolCall(tool_call) => {
                assert_eq!(tool_call.invocation.name, "bash");
                assert_eq!(tool_call.invocation.input["cmd"], "ls");
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        assert_eq!(response.text, "Hello");
        assert_eq!(response.model, ModelId::new(MODEL_SONNET_4_6));
        assert_eq!(response.input_tokens, 15);
        assert_eq!(response.cached_input_tokens, 3);
        assert_eq!(response.output_tokens, 5);
        assert_eq!(response.usage.input_tokens_uncached, 12);
        assert_eq!(response.usage.input_tokens_cache_write, 0);
        assert_eq!(response.usage.input_tokens_cache_read, 3);
        assert!(matches!(response.stop_reason, StopReason::ToolUse));
    }

    #[test]
    fn supported_models_return_expected_capabilities() {
        let opus_caps =
            capabilities_for_model(&canonical_model_id(MODEL_OPUS_4_6).unwrap()).unwrap();
        let sonnet_caps =
            capabilities_for_model(&canonical_model_id(MODEL_SONNET_4_6).unwrap()).unwrap();

        assert_eq!(opus_caps.context_window, 1_000_000);
        assert_eq!(sonnet_caps.context_window, 1_000_000);
        assert_eq!(opus_caps.max_output, 128_000);
        assert_eq!(sonnet_caps.max_output, 64_000);
        assert!((opus_caps.pricing.input_per_mtok - 5.0_f64).abs() < f64::EPSILON);
        assert!((sonnet_caps.pricing.input_per_mtok - 3.0_f64).abs() < f64::EPSILON);
        assert_eq!(opus_caps.model_id, ModelId::new(MODEL_OPUS_4_6));
        assert_eq!(sonnet_caps.model_id, ModelId::new(MODEL_SONNET_4_6));
    }

    #[test]
    fn model_ids_resolve_without_aliasing() {
        assert_eq!(canonical_model_id(MODEL_OPUS_4_6).unwrap(), MODEL_OPUS_4_6);
        assert_eq!(
            canonical_model_id(MODEL_SONNET_4_6).unwrap(),
            MODEL_SONNET_4_6
        );
    }

    #[test]
    fn provider_accepts_documented_default_models() {
        let provider = AnthropicProvider::new("test-key", MODEL_SONNET_4_6).unwrap();
        assert_eq!(
            provider.capabilities().model_id,
            ModelId::new(MODEL_SONNET_4_6)
        );
    }
}
