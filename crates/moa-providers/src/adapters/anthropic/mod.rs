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
    CompletionResponse, CompletionStream, ContextMessage, JsonResponseFormat, LLMProvider,
    MessageRole, MoaConfig, MoaError, ModelCapabilities, ModelId, ProviderNativeTool, Result,
    StopReason, TokenPricing, TokenUsage, ToolCallFormat, ToolContent, ToolInvocation,
    estimate_text_tokens,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::core::http::build_http_client;
use crate::core::instrumentation::LLMSpanRecorder;
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
use request::build_request_body;
use streaming::consume_sse_events;

/// Builds an Anthropic request body for inspection in integration tests without sending it.
#[cfg(feature = "test-util")]
pub fn debug_build_anthropic_request_body(
    request: &CompletionRequest,
    web_search_enabled: bool,
) -> Result<Value> {
    request::debug_build_anthropic_request_body(request, web_search_enabled)
}

#[cfg(test)]
use tools::{anthropic_content_blocks, anthropic_message, anthropic_tool_from_schema};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_STREAM_BUFFER: usize = 64;
const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 4_096;
const MAX_CACHE_BREAKPOINTS: usize = 4;
const MIN_CACHEABLE_TOKENS: usize = 1_024;
const MODEL_HAIKU_4_5: &str = "claude-haiku-4-5";
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

    /// Clones this provider while swapping the default model id.
    pub(crate) fn clone_with_model(&self, default_model: impl Into<String>) -> Result<Self> {
        let default_model = canonical_model_id(&default_model.into())?;
        let default_capabilities = capabilities_for_model(&default_model)?;
        Ok(Self {
            client: self.client.clone(),
            api_key: self.api_key.clone(),
            default_model,
            default_capabilities,
            messages_url: self.messages_url.clone(),
            retry_policy: self.retry_policy.clone(),
            web_search_enabled: self.web_search_enabled,
        })
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
