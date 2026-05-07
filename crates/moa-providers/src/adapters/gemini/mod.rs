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
use std::sync::Arc;
use std::time::Instant;

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_util::{Stream, StreamExt, pin_mut};
use moa_core::{
    CacheTtl, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    ContextMessage, JsonResponseFormat, LLMProvider, MessageRole, MoaConfig, MoaError,
    ModelCapabilities, ModelId, ProviderNativeTool, ProviderToolCallMetadata, Result, StopReason,
    TokenPricing, TokenUsage, ToolCallContent, ToolCallFormat, ToolContent, ToolInvocation,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;

use crate::core::http::build_http_client;
use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::provider_tools::enabled_native_tools;
use crate::core::retry::RetryPolicy;
use crate::core::streaming::parse_sse_json;

mod model;
mod request;
mod response;
mod streaming;
mod tools;

#[cfg(test)]
mod tests;

use model::{canonical_model_id, capabilities_for_model};
use streaming::consume_sse_events;

#[cfg(test)]
use request::{build_explicit_cache_plan, build_request_body};
#[cfg(test)]
use response::{GeminiUsageMetadata, token_usage_from_gemini_usage};
#[cfg(test)]
use tools::thinking_config_for_model;

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8_192;
const DEFAULT_MAX_RETRIES: usize = 3;

/// Google Gemini provider backed by `streamGenerateContent`.
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: Arc<str>,
    api_base: Arc<str>,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
    explicit_cache_names: Mutex<HashMap<String, String>>,
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
            api_key: Arc::from(api_key.into()),
            api_base: Arc::from(GEMINI_API_BASE),
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            retry_policy: RetryPolicy::default().with_max_retries(DEFAULT_MAX_RETRIES),
            web_search_enabled: true,
            explicit_cache_names: Mutex::new(HashMap::new()),
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

    /// Clones this provider while swapping the default model id.
    pub(crate) fn clone_with_model(&self, default_model: impl Into<String>) -> Result<Self> {
        let default_model = canonical_model_id(&default_model.into())?;
        let default_capabilities = capabilities_for_model(&default_model);
        Ok(Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            api_base: self.api_base.clone(),
            default_model,
            default_reasoning_effort: self.default_reasoning_effort.clone(),
            default_capabilities,
            retry_policy: self.retry_policy.clone(),
            web_search_enabled: self.web_search_enabled,
            explicit_cache_names: Mutex::new(HashMap::new()),
        })
    }

    /// Overrides the Gemini REST API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = Arc::from(api_base.into().as_str());
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
            .as_ref()
            .map(ModelId::as_str)
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
        let native_tools = enabled_native_tools(&model_capabilities, self.web_search_enabled);
        let span_recorder = LLMSpanRecorder::new(
            "google",
            resolved_model.clone(),
            &request,
            max_output_tokens,
            model_capabilities.pricing.clone(),
        );
        span_recorder.set_phase("build_request");
        let span = span_recorder.span().clone();
        let request_body = match self
            .build_request_body_with_cache(
                &request,
                &resolved_model,
                &self.default_reasoning_effort,
                native_tools,
            )
            .await
        {
            Ok(body) => body,
            Err(error) => {
                span_recorder.fail_at_stage("build_request", &error);
                return Err(error);
            }
        };

        let client = self.client.clone();
        let api_key = Arc::clone(&self.api_key);
        let api_base = Arc::clone(&self.api_base);
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
                            .header("x-goog-api-key", &*api_key)
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
