//! Anthropic Claude provider implementation with SSE streaming support.
//!
//! Internal adapter phases:
//! 1. build one Anthropic Messages request body
//! 2. execute provider transport with shared retry handling
//! 3. normalize SSE events into `CompletionContent`
//! 4. finalize one normalized `CompletionResponse`

use std::env;
use std::sync::Arc;
use std::time::Instant;

use eventsource_stream::{Event as SseEvent, Eventsource};
use futures_util::{Stream, pin_mut};
use moa_core::{
    config::{MoaConfig, ProviderStreamTimeoutConfig},
    error::MoaError,
    error::Result,
    traits::LLMProvider,
    types::completion::CompletionContent,
    types::completion::CompletionRequest,
    types::completion::CompletionResponse,
    types::completion::CompletionStream,
    types::completion::JsonResponseFormat,
    types::completion::StopReason,
    types::completion::TokenUsage,
    types::completion::ToolInvocation,
    types::context::ContextMessage,
    types::context::MessageRole,
    types::context::estimate_text_tokens,
    types::identifiers::ModelId,
    types::model::ModelCapabilities,
    types::model::ProviderNativeTool,
    types::tools::ToolContent,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::concurrency_factory::{CallKind, ProviderConcurrency};
use crate::core::http::build_http_client;
use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::rate_guard::{self, RateGuard};
use crate::core::retry::RetryPolicy;
use crate::core::streaming::{
    StreamDeadline, finalize_streamed_completion, parse_sse_json, send_with_transport_phase,
};

pub(crate) mod model;
mod request;
mod response;
mod streaming;
mod tools;

#[cfg(test)]
mod tests;

use model::{canonical_model_id, capabilities_for_model};
use request::build_request_body;
use streaming::consume_sse_events;

pub use request::debug_build_anthropic_request_body;

#[cfg(test)]
use tools::{anthropic_content_blocks, anthropic_message, anthropic_tool_from_schema};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_STREAM_BUFFER: usize = 64;
const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 4_096;
const MIN_CACHEABLE_TOKENS: usize = 1_024;
#[cfg(test)]
const MODEL_HAIKU_4_5: &str = "claude-haiku-4-5";
#[cfg(test)]
const MODEL_OPUS_4_6: &str = "claude-opus-4-6";
const MODEL_SONNET_4_6: &str = "claude-sonnet-4-6";

/// Anthropic Claude provider backed by the Messages API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: Arc<str>,
    default_model: String,
    default_capabilities: ModelCapabilities,
    messages_url: Arc<str>,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
    guard: RateGuard,
    stream_timeouts: ProviderStreamTimeoutConfig,
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
            pacer: RatePacer::new(PacerConfig::disabled()),
            // Direct construction uses the flat per-provider default; the config
            // path overrides it per credential (0 opts back into unbounded).
            limiter: ConcurrencyLimiter::new(DEFAULT_MAX_IN_FLIGHT),
            guard: RateGuard::new(),
            stream_timeouts: ProviderStreamTimeoutConfig::default(),
        })
    }

    /// Creates a provider from the configured Anthropic environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.models.main.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_ANTHROPIC_API_KEY",
            &config.providers.anthropic.api_key,
        )?;

        let mut provider = Self::new(api_key.clone(), default_model)?
            .with_web_search_enabled(config.general.web_search_enabled);
        if let Some(max) = config.providers.anthropic.max_requests_per_min {
            provider = provider.with_rate_limits(PacerConfig::requests_per_min(max));
        }
        provider.limiter = ProviderConcurrency::from_config(config).limiter(
            CallKind::Chat,
            "anthropic",
            &api_key,
            config.providers.anthropic.max_concurrent_requests,
        );
        provider.stream_timeouts = config.providers.stream_timeouts;
        Ok(provider)
    }

    /// Creates a provider from the `MOA_ANTHROPIC_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("MOA_ANTHROPIC_API_KEY").map_err(|_| {
            MoaError::MissingEnvironmentVariable("MOA_ANTHROPIC_API_KEY".to_string())
        })?;

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

    /// Overrides request-per-minute pacing for this provider instance.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
        self
    }

    /// Caps the number of in-flight completions this provider keeps open at once.
    ///
    /// Unbounded by default; a configured ceiling makes queued generations wait
    /// for a slot before dispatching.
    #[must_use]
    pub fn with_max_concurrent_requests(mut self, max_in_flight: usize) -> Self {
        self.limiter = ConcurrencyLimiter::new(max_in_flight);
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
        // Cooperative 429 short-circuit: while paused, return a typed rate-limit
        // error immediately without an HTTP round trip so callers can fail over.
        if let Some(remaining) = self.guard.pause_remaining() {
            return Err(rate_guard::rate_limited_paused(remaining));
        }
        self.guard.note_request();

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
        // Take an in-flight slot before dispatching; a gate that stays saturated
        // past the block threshold is a failover-eligible block, not a queue.
        let permit = match self.limiter.acquire().await {
            Some(lease) => lease,
            None => {
                let error = rate_guard::rate_limited_saturated(self.limiter.block_threshold());
                span_recorder.fail_at_stage("transport", &error);
                return Err(error);
            }
        };
        // Chat completions are request-rate limited; pace after taking the
        // in-flight slot so queued callers do not consume rate budget early.
        self.pacer.acquire(1, 0).await;
        let client = self.client.clone();
        let api_key = Arc::clone(&self.api_key);
        let messages_url = Arc::clone(&self.messages_url);
        let retry_policy = self.retry_policy.clone();
        let guard = self.guard.clone();
        let stream_timeouts = self.stream_timeouts;
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                let mut span_recorder = span_recorder;
                // Hold the in-flight slot for the whole generation; released when
                // the streamed completion finishes.
                let _permit = permit;
                let started_at = Instant::now();
                let response =
                    send_with_transport_phase(&span_recorder, &retry_policy, &guard, || {
                        client
                            .post(&*messages_url)
                            .header("x-api-key", &*api_key)
                            .header("anthropic-version", ANTHROPIC_API_VERSION)
                            .header(ACCEPT, "text/event-stream")
                            .header(CONTENT_TYPE, "application/json")
                            .json(&request_body)
                    })
                    .await?;

                span_recorder.set_phase("stream");
                let consumed = consume_sse_events(
                    response.bytes_stream().eventsource(),
                    tx,
                    resolved_model,
                    started_at,
                    &mut span_recorder,
                    stream_timeouts,
                )
                .await;

                finalize_streamed_completion(&span_recorder, consumed)
            }
            .instrument(span),
        );

        Ok(CompletionStream::new(rx, completion_task))
    }
}
