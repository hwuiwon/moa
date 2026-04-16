//! Google Gemini provider implementation using the Gemini REST API.
//!
//! Internal adapter phases:
//! 1. build one Gemini `streamGenerateContent` request body
//! 2. execute provider transport with shared retry handling
//! 3. normalize SSE events into `CompletionContent`
//! 4. finalize one normalized `CompletionResponse`
//! 5. record provider-private stream snapshots for tracing/debugging

use std::collections::HashMap;
use std::env;
use std::time::Instant;

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_util::{Stream, StreamExt, pin_mut};
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, ContextMessage,
    LLMProvider, MessageRole, MoaConfig, MoaError, ModelCapabilities, ProviderNativeTool,
    ProviderToolCallMetadata, Result, StopReason, TokenPricing, ToolCallContent, ToolCallFormat,
    ToolContent, ToolInvocation,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::http::build_http_client;
use crate::instrumentation::LLMSpanRecorder;
use crate::provider_tools::{
    enabled_native_tools, web_search_completed_block, web_search_started_block,
};
use crate::retry::RetryPolicy;
use crate::schema::compile_for_gemini;
use crate::sse::parse_sse_json;

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8_192;
const DEFAULT_MAX_RETRIES: usize = 3;

/// Google Gemini provider backed by `streamGenerateContent`.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
}

impl GeminiProvider {
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
        let default_model = canonical_model_id(&default_model.into())?;
        let default_capabilities = capabilities_for_model(&default_model);

        Ok(Self {
            client: build_http_client()?,
            api_key: api_key.into(),
            api_base: GEMINI_API_BASE.to_string(),
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            retry_policy: RetryPolicy::default().with_max_retries(DEFAULT_MAX_RETRIES),
            web_search_enabled: true,
        })
    }

    /// Creates a provider from the configured Google Gemini environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.general.default_model.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key_env = config.providers.google.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;

        Self::new_with_reasoning_effort(
            api_key,
            default_model,
            config.general.reasoning_effort.clone(),
        )
        .map(|provider| provider.with_web_search_enabled(config.general.web_search_enabled))
    }

    /// Creates a provider from the `GOOGLE_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("GOOGLE_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("GOOGLE_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the Gemini REST API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    /// Overrides the retry budget for retryable provider failures.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.retry_policy = self.retry_policy.with_max_retries(max_retries);
        self
    }

    /// Overrides whether provider-native Google Search is exposed to supported models.
    pub fn with_web_search_enabled(mut self, enabled: bool) -> Self {
        self.web_search_enabled = enabled;
        self
    }
}

#[async_trait::async_trait]
impl LLMProvider for GeminiProvider {
    fn name(&self) -> &str {
        "google"
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
        let resolved_model = canonical_model_id(&requested_model)?;
        let model_capabilities = capabilities_for_model(&resolved_model);
        let max_output_tokens = Some(
            request
                .max_output_tokens
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
                .min(model_capabilities.max_output),
        );
        let span_recorder = LLMSpanRecorder::new(
            "google",
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
            &self.default_reasoning_effort,
            enabled_native_tools(&model_capabilities, self.web_search_enabled),
        ) {
            Ok(body) => body,
            Err(error) => {
                span_recorder.fail_at_stage("build_request", &error);
                return Err(error);
            }
        };

        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let api_base = self.api_base.clone();
        let retry_policy = self.retry_policy.clone();
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                let mut span_recorder = span_recorder;
                let started_at = Instant::now();
                let url = format!(
                    "{}/models/{}:streamGenerateContent?alt=sse",
                    api_base.trim_end_matches('/'),
                    resolved_model
                );

                span_recorder.set_phase("transport");
                let response = retry_policy
                    .send(|| {
                        client
                            .post(&url)
                            .header("x-goog-api-key", &api_key)
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
    let model = model.trim();
    if model.starts_with("gemini-") {
        return Ok(model.to_string());
    }

    Err(MoaError::Unsupported(format!(
        "unsupported Google Gemini model '{model}'"
    )))
}

fn capabilities_for_model(model: &str) -> ModelCapabilities {
    if model.starts_with("gemini-3.1-pro") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 2.0,
                output_per_mtok: 12.0,
                cached_input_per_mtok: Some(0.2),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-3-flash") || model.starts_with("gemini-3.1-flash") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 0.5,
                output_per_mtok: 3.0,
                cached_input_per_mtok: Some(0.05),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-2.5-pro") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 65_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 1.25,
                output_per_mtok: 10.0,
                cached_input_per_mtok: Some(0.125),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-2.5-flash") {
        return ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 65_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 0.3,
                output_per_mtok: 2.5,
                cached_input_per_mtok: Some(0.03),
            },
            native_tools: native_google_search_tools(),
        };
    }

    ModelCapabilities {
        model_id: model.to_string(),
        context_window: 1_000_000,
        max_output: 65_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
        },
        native_tools: native_google_search_tools(),
    }
}

fn build_request_body(
    request: &CompletionRequest,
    model: &str,
    default_reasoning_effort: &str,
    native_tools: &[ProviderNativeTool],
) -> Result<Value> {
    let (system_instruction, contents) = build_contents(request)?;
    let function_declarations = request
        .tools
        .iter()
        .map(gemini_function_declaration)
        .collect::<Result<Vec<_>>>()?;

    if contents.is_empty() {
        return Err(MoaError::ValidationError(
            "Gemini requests require at least one non-system message".to_string(),
        ));
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));
    if let Some(system_instruction) = system_instruction {
        body.insert("systemInstruction".to_string(), system_instruction);
    }

    let mut generation_config = Map::new();
    if let Some(max_output_tokens) = request.max_output_tokens {
        generation_config.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
    }
    if let Some(temperature) = request.temperature {
        generation_config.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(thinking_config) = thinking_config_for_model(model, default_reasoning_effort)? {
        generation_config.insert("thinkingConfig".to_string(), thinking_config);
    }
    if !generation_config.is_empty() {
        body.insert(
            "generationConfig".to_string(),
            Value::Object(generation_config),
        );
    }

    let mut tools = Vec::new();
    let has_function_declarations = !function_declarations.is_empty();
    if has_function_declarations {
        tools.push(json!({ "functionDeclarations": function_declarations }));
    } else {
        for tool in native_tools {
            if tool.tool_type == "google_search" {
                tools.push(json!({
                    "google_search": tool.config.clone().unwrap_or_else(|| json!({}))
                }));
            }
        }
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }

    Ok(Value::Object(body))
}

fn build_contents(request: &CompletionRequest) -> Result<(Option<Value>, Vec<Value>)> {
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    let mut model_parts = Vec::new();
    let mut tool_response_parts = Vec::new();
    let mut tool_names_by_id = HashMap::new();

    for message in &request.messages {
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
    Ok((system_instruction, contents))
}

fn flush_pending_parts(
    contents: &mut Vec<Value>,
    model_parts: &mut Vec<Value>,
    tool_response_parts: &mut Vec<Value>,
) {
    if !model_parts.is_empty() {
        contents.push(content_message("model", std::mem::take(model_parts)));
    }
    flush_tool_responses(contents, tool_response_parts);
}

fn flush_tool_responses(contents: &mut Vec<Value>, tool_response_parts: &mut Vec<Value>) {
    if !tool_response_parts.is_empty() {
        contents.push(content_message("user", std::mem::take(tool_response_parts)));
    }
}

fn content_message(role: &str, parts: Vec<Value>) -> Value {
    json!({
        "role": role,
        "parts": parts,
    })
}

fn text_part(text: &str, thought_signature: Option<&str>) -> Value {
    let mut part = Map::new();
    part.insert("text".to_string(), Value::String(text.to_string()));
    if let Some(thought_signature) = thought_signature {
        part.insert(
            "thoughtSignature".to_string(),
            Value::String(thought_signature.to_string()),
        );
    }
    Value::Object(part)
}

fn function_call_part(invocation: &ToolInvocation, thought_signature: Option<&str>) -> Value {
    let mut function_call = Map::new();
    function_call.insert("name".to_string(), Value::String(invocation.name.clone()));
    function_call.insert("args".to_string(), normalize_tool_args(&invocation.input));
    if let Some(id) = invocation.id.as_ref() {
        function_call.insert("id".to_string(), Value::String(id.clone()));
    }

    let mut part = Map::new();
    part.insert("functionCall".to_string(), Value::Object(function_call));
    if let Some(thought_signature) = thought_signature {
        part.insert(
            "thoughtSignature".to_string(),
            Value::String(thought_signature.to_string()),
        );
    }
    Value::Object(part)
}

fn function_response_part(name: &str, call_id: &str, message: &ContextMessage) -> Value {
    json!({
        "functionResponse": {
            "name": name,
            "id": call_id,
            "response": function_response_payload(message),
        }
    })
}

fn function_response_payload(message: &ContextMessage) -> Value {
    match message.content_blocks.as_ref() {
        Some(blocks) if blocks.len() == 1 => match &blocks[0] {
            ToolContent::Text { text } => json!({ "result": text }),
            ToolContent::Json { data } => json!({ "result": data }),
        },
        Some(blocks) if !blocks.is_empty() => json!({
            "result": {
                "text": message.content,
                "content": blocks.iter().map(tool_content_value).collect::<Vec<_>>(),
            }
        }),
        _ => json!({ "result": message.content }),
    }
}

fn tool_content_value(content: &ToolContent) -> Value {
    match content {
        ToolContent::Text { text } => json!({ "text": text }),
        ToolContent::Json { data } => data.clone(),
    }
}

fn gemini_function_declaration(schema: &Value) -> Result<Value> {
    let function = schema.get("function").unwrap_or(schema);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MoaError::ValidationError("tool schema is missing a function name".to_string())
        })?;
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let parameters = function
        .get("parameters")
        .or_else(|| function.get("input_schema"))
        .map(compile_for_gemini)
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));

    Ok(json!({
        "name": name,
        "description": description,
        "parameters": parameters,
    }))
}

fn thinking_config_for_model(model: &str, reasoning_effort: &str) -> Result<Option<Value>> {
    let effort = normalize_reasoning_effort(reasoning_effort)?;

    if model.starts_with("gemini-3") {
        let level = if model.contains("flash") {
            match effort {
                ReasoningEffort::None | ReasoningEffort::Minimal => "minimal",
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                ReasoningEffort::High | ReasoningEffort::Xhigh => "high",
            }
        } else {
            match effort {
                ReasoningEffort::High | ReasoningEffort::Xhigh => "high",
                ReasoningEffort::None
                | ReasoningEffort::Minimal
                | ReasoningEffort::Low
                | ReasoningEffort::Medium => "low",
            }
        };
        return Ok(Some(json!({ "thinkingLevel": level })));
    }

    if model.starts_with("gemini-2.5") {
        let budget = if model.contains("flash") {
            match effort {
                ReasoningEffort::None | ReasoningEffort::Minimal => 0,
                ReasoningEffort::Low => 1_024,
                ReasoningEffort::Medium => 4_096,
                ReasoningEffort::High => 8_192,
                ReasoningEffort::Xhigh => 16_384,
            }
        } else {
            match effort {
                ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Low => 128,
                ReasoningEffort::Medium => 4_096,
                ReasoningEffort::High => 16_384,
                ReasoningEffort::Xhigh => 32_768,
            }
        };
        return Ok(Some(json!({ "thinkingBudget": budget })));
    }

    Ok(None)
}

fn normalize_tool_args(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        _ => json!({ "value": input }),
    }
}

fn native_google_search_tools() -> Vec<ProviderNativeTool> {
    vec![ProviderNativeTool {
        tool_type: "google_search".to_string(),
        name: "web_search".to_string(),
        config: Some(json!({})),
    }]
}

fn is_standard_user_message(message: &ContextMessage) -> bool {
    message.role == MessageRole::User
        && message.tool_invocation.is_none()
        && message.tool_use_id.is_none()
}

#[derive(Debug, Clone, Copy)]
enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

fn normalize_reasoning_effort(value: &str) -> Result<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(ReasoningEffort::None),
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::Xhigh),
        other => Err(MoaError::ConfigError(format!(
            "unsupported Gemini reasoning effort '{other}'"
        ))),
    }
}

async fn consume_sse_events<S>(
    stream: S,
    tx: mpsc::Sender<Result<CompletionContent>>,
    fallback_model: String,
    started_at: Instant,
    span_recorder: &mut LLMSpanRecorder,
) -> Result<CompletionResponse>
where
    S: Stream<
        Item = std::result::Result<SseEvent, eventsource_stream::EventStreamError<reqwest::Error>>,
    >,
{
    let mut state = GeminiStreamState::new(fallback_model);
    pin_mut!(stream);

    while let Some(event) = stream.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                return Err(MoaError::StreamError(format!(
                    "failed to read Gemini SSE event: {error}"
                )));
            }
        };

        for block in state.apply_event(&event)? {
            span_recorder.observe_block(&block);
            if tx.send(Ok(block)).await.is_err() {
                tracing::debug!("completion stream receiver dropped before the response finished");
                span_recorder.record_raw_response(&state.debug_snapshot());
                return Ok(state.finish(started_at));
            }
        }
    }

    span_recorder.record_raw_response(&state.debug_snapshot());
    Ok(state.finish(started_at))
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiGenerateContentResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default, rename = "modelVersion")]
    model_version: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
    #[serde(default, rename = "groundingMetadata")]
    grounding_metadata: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(default, rename = "thoughtSignature")]
    thought_signature: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<usize>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<usize>,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached_content_token_count: Option<usize>,
}

#[derive(Debug)]
struct GeminiStreamState {
    model: String,
    text: String,
    content: Vec<CompletionContent>,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    stop_reason: StopReason,
    thought_signature: Option<String>,
    search_started_emitted: bool,
    search_completed_emitted: bool,
    last_raw_response: Option<GeminiGenerateContentResponse>,
}

impl GeminiStreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            text: String::new(),
            content: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            stop_reason: StopReason::EndTurn,
            thought_signature: None,
            search_started_emitted: false,
            search_completed_emitted: false,
            last_raw_response: None,
        }
    }

    fn apply_event(&mut self, event: &SseEvent) -> Result<Vec<CompletionContent>> {
        let response: GeminiGenerateContentResponse = parse_sse_json(event)?;
        self.last_raw_response = Some(response.clone());
        if let Some(model_version) = response.model_version.clone()
            && !model_version.is_empty()
        {
            self.model = model_version;
        }
        if let Some(usage) = response.usage_metadata {
            self.input_tokens = usage.prompt_token_count.unwrap_or(self.input_tokens);
            self.output_tokens = usage.candidates_token_count.unwrap_or(self.output_tokens);
            self.cached_input_tokens = usage
                .cached_content_token_count
                .unwrap_or(self.cached_input_tokens);
        }

        let mut emitted = Vec::new();
        for candidate in response.candidates {
            if candidate.grounding_metadata.is_some() && !self.search_started_emitted {
                self.search_started_emitted = true;
                let block = web_search_started_block();
                self.content.push(block.clone());
                emitted.push(block);
            }

            if let Some(content) = candidate.content {
                for part in content.parts {
                    if let Some(function_call) = part.function_call {
                        let call = CompletionContent::ToolCall(ToolCallContent {
                            invocation: ToolInvocation {
                                id: function_call.id.clone(),
                                name: function_call.name,
                                input: normalize_tool_args(&function_call.args),
                            },
                            provider_metadata: part.thought_signature.clone().map(
                                |thought_signature| ProviderToolCallMetadata::Gemini {
                                    thought_signature,
                                },
                            ),
                        });
                        self.content.push(call.clone());
                        emitted.push(call);
                        continue;
                    }

                    if let Some(text) = part.text
                        && !text.is_empty()
                    {
                        self.text.push_str(&text);
                        let block = CompletionContent::Text(text);
                        self.content.push(block.clone());
                        emitted.push(block);
                    }

                    if part.thought_signature.is_some() {
                        self.thought_signature = part.thought_signature;
                    }
                }
            }

            if let Some(finish_reason) = candidate.finish_reason.as_deref() {
                self.stop_reason = finish_reason_to_stop_reason(finish_reason);
                if candidate.grounding_metadata.is_some() && !self.search_completed_emitted {
                    self.search_completed_emitted = true;
                    let block = web_search_completed_block();
                    self.content.push(block.clone());
                    emitted.push(block);
                }
            }
        }

        Ok(emitted)
    }

    fn finish(mut self, started_at: Instant) -> CompletionResponse {
        if self
            .content
            .iter()
            .any(|entry| matches!(entry, CompletionContent::ToolCall(_)))
        {
            self.stop_reason = StopReason::ToolUse;
        }

        if self.content.is_empty() && !self.text.is_empty() {
            self.content
                .push(CompletionContent::Text(self.text.clone()));
        }

        CompletionResponse {
            text: self.text,
            content: self.content,
            stop_reason: self.stop_reason,
            model: self.model,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            duration_ms: started_at.elapsed().as_millis() as u64,
            thought_signature: self.thought_signature,
        }
    }

    fn debug_snapshot(&self) -> GeminiStreamDebugSnapshot {
        GeminiStreamDebugSnapshot {
            model: self.model.clone(),
            stop_reason: self.stop_reason.clone(),
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_input_tokens: self.cached_input_tokens,
            thought_signature: self.thought_signature.clone(),
            content: self.content.clone(),
            last_raw_response: self.last_raw_response.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct GeminiStreamDebugSnapshot {
    model: String,
    stop_reason: StopReason,
    input_tokens: usize,
    output_tokens: usize,
    cached_input_tokens: usize,
    thought_signature: Option<String>,
    content: Vec<CompletionContent>,
    last_raw_response: Option<GeminiGenerateContentResponse>,
}

fn finish_reason_to_stop_reason(finish_reason: &str) -> StopReason {
    match finish_reason {
        "MAX_TOKENS" => StopReason::MaxTokens,
        "CANCELLED" => StopReason::Cancelled,
        "STOP" => StopReason::EndTurn,
        other => StopReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        CompletionContent, CompletionRequest, ContextMessage, LLMProvider,
        ProviderToolCallMetadata, ToolCallContent, ToolContent, ToolInvocation,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::GeminiProvider;

    fn sse_stream(frames: &[Value]) -> String {
        let mut stream = String::new();
        for frame in frames {
            stream.push_str("data: ");
            stream.push_str(&frame.to_string());
            stream.push_str("\n\n");
        }
        stream
    }

    #[tokio::test]
    async fn gemini_provider_serializes_system_messages_and_tools() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 16384];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();

            assert!(
                request
                    .contains("POST /v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse")
            );
            assert!(
                request.contains(
                    "\"systemInstruction\":{\"parts\":[{\"text\":\"Follow the rules.\"}]}"
                )
            );
            assert!(request.contains("\"role\":\"user\""));
            assert!(request.contains("\"text\":\"hello\""));
            assert!(request.contains(
                "\"functionDeclarations\":[{\"description\":\"Read a file\",\"name\":\"file_read\""
            ));
            assert!(!request.contains("\"additionalProperties\":false"));
            assert!(!request.contains("\"google_search\":{}"));
            assert!(request.contains("\"thinkingBudget\":4096"));

            let body = sse_stream(&[json!({
                "candidates": [{
                    "content": {
                        "parts": [{ "text": "ok" }]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 8,
                    "candidatesTokenCount": 2,
                    "cachedContentTokenCount": 1
                },
                "modelVersion": "gemini-2.5-flash"
            })]);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash")
            .unwrap()
            .with_api_base(format!("http://{address}/v1beta"))
            .with_max_retries(0);
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![
                    ContextMessage::system("Follow the rules."),
                    ContextMessage::user("hello"),
                ],
                tools: vec![json!({
                    "name": "file_read",
                    "description": "Read a file",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        },
                        "additionalProperties": false,
                        "required": ["path"]
                    }
                })],
                max_output_tokens: Some(1024),
                temperature: Some(0.2),
                cache_breakpoints: Vec::new(),
                metadata: Default::default(),
            })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        assert_eq!(response.text, "ok");
        assert_eq!(response.model, "gemini-2.5-flash");
        server.abort();
    }

    #[tokio::test]
    async fn gemini_provider_groups_tool_history_and_preserves_thought_signatures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 16384];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();

            assert!(request.contains("\"functionCall\":{\"args\":{\"path\":\"notes/today.md\"},\"id\":\"fc_1\",\"name\":\"file_write\"}"));
            assert!(request.contains("\"thoughtSignature\":\"sig_fc_1\""));
            assert!(
                request.contains("\"functionResponse\":{\"id\":\"fc_1\",\"name\":\"file_write\"")
            );
            assert!(request.contains("\"result\":{\"path\":\"notes/today.md\"}"));

            let body = sse_stream(&[json!({
                "candidates": [{
                    "content": {
                        "parts": [{ "text": "done" }]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 8,
                    "candidatesTokenCount": 2
                },
                "modelVersion": "gemini-2.5-flash"
            })]);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash")
            .unwrap()
            .with_api_base(format!("http://{address}/v1beta"))
            .with_max_retries(0);
        let response = provider
            .complete(CompletionRequest {
                model: None,
                messages: vec![
                    ContextMessage::user("write the file"),
                    ContextMessage::assistant_tool_call_with_thought_signature(
                        ToolInvocation {
                            id: Some("fc_1".to_string()),
                            name: "file_write".to_string(),
                            input: json!({ "path": "notes/today.md" }),
                        },
                        "<tool_call />",
                        Some("sig_fc_1"),
                    ),
                    ContextMessage::tool_result(
                        "fc_1",
                        "ok",
                        Some(vec![ToolContent::Json {
                            data: json!({ "path": "notes/today.md" }),
                        }]),
                    ),
                ],
                tools: Vec::new(),
                max_output_tokens: Some(1024),
                temperature: None,
                cache_breakpoints: Vec::new(),
                metadata: Default::default(),
            })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        assert_eq!(response.text, "done");
        server.abort();
    }

    #[tokio::test]
    async fn gemini_provider_serializes_google_search_without_functions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 16384];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();

            assert!(request.contains("\"google_search\":{}"));
            assert!(!request.contains("\"functionDeclarations\""));

            let body = sse_stream(&[json!({
                "candidates": [{
                    "content": {
                        "parts": [{ "text": "headline" }]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 8,
                    "candidatesTokenCount": 2
                },
                "modelVersion": "gemini-2.5-flash"
            })]);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash")
            .unwrap()
            .with_api_base(format!("http://{address}/v1beta"))
            .with_max_retries(0);
        let response = provider
            .complete(CompletionRequest::simple("latest news"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        assert_eq!(response.text, "headline");
        server.abort();
    }

    #[tokio::test]
    async fn gemini_provider_streams_tool_calls_and_google_search_updates() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();

            let body = sse_stream(&[
                json!({
                    "candidates": [{
                        "content": {
                            "parts": [{
                                "functionCall": {
                                    "id": "fc_stream_1",
                                    "name": "emit_token",
                                    "args": { "token": "LIVE" }
                                },
                                "thoughtSignature": "sig_stream_1"
                            }]
                        }
                    }]
                }),
                json!({
                    "candidates": [{
                        "content": {
                            "parts": [{ "text": "headline" }]
                        },
                        "groundingMetadata": {
                            "webSearchQueries": ["latest headline"]
                        },
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 11,
                        "candidatesTokenCount": 3
                    },
                    "modelVersion": "gemini-2.5-flash"
                }),
            ]);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let provider = GeminiProvider::new("test-key", "gemini-2.5-flash")
            .unwrap()
            .with_api_base(format!("http://{address}/v1beta"))
            .with_max_retries(0);
        let response = provider
            .complete(CompletionRequest::simple("latest news"))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();

        assert_eq!(response.text, "headline");
        assert!(response.content.iter().any(|content| {
            matches!(
                content,
                CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation { id, name, .. },
                    provider_metadata: Some(ProviderToolCallMetadata::Gemini { thought_signature }),
                }) if id.as_deref() == Some("fc_stream_1")
                    && name == "emit_token"
                    && thought_signature == "sig_stream_1"
            )
        }));
        assert!(response.content.iter().any(|content| {
            matches!(
                content,
                CompletionContent::ProviderToolResult { tool_name, .. } if tool_name == "web_search"
            )
        }));
        server.abort();
    }
}
