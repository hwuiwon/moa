//! Google Gemini provider implementation using the Gemini REST API.
//!
//! Internal adapter phases:
//! 1. build one Gemini `streamGenerateContent` request body
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
    types::completion::ProviderToolCallMetadata,
    types::completion::StopReason,
    types::completion::TokenUsage,
    types::completion::ToolCallContent,
    types::completion::ToolInvocation,
    types::context::ContextMessage,
    types::context::MessageRole,
    types::identifiers::ModelId,
    types::model::ModelCapabilities,
    types::model::ProviderNativeTool,
    types::tools::ToolContent,
};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_MAX_IN_FLIGHT};
use crate::core::concurrency_factory::{CallKind, ProviderConcurrency};
use crate::core::http::build_http_client;
use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::provider_tools::enabled_native_tools;
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
use streaming::consume_sse_events;

#[cfg(test)]
use request::build_request_body;
#[cfg(test)]
use response::{GeminiUsageMetadata, token_usage_from_gemini_usage};
#[cfg(test)]
use tools::thinking_config_for_model;

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 8_192;
const DEFAULT_MAX_RETRIES: usize = 3;

/// Builds a Gemini request body for inspection tests without sending it.
pub fn debug_build_gemini_request_body(
    request: &CompletionRequest,
    web_search_enabled: bool,
) -> Result<Value> {
    let requested_model = request
        .model
        .as_ref()
        .map(ModelId::as_str)
        .unwrap_or("gemini-3-flash-preview");
    let resolved_model = canonical_model_id(requested_model)?;
    let capabilities = capabilities_for_model(&resolved_model)?;
    request::build_request_body(
        request,
        &resolved_model,
        "medium",
        enabled_native_tools(
            &capabilities,
            web_search_enabled
                && request.native_web_search
                    != moa_core::types::completion::NativeWebSearchPolicy::Disabled,
        ),
    )
}

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
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
    guard: RateGuard,
    stream_timeouts: ProviderStreamTimeoutConfig,
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
        let default_capabilities = capabilities_for_model(&default_model)?;

        Ok(Self {
            client: build_http_client()?,
            api_key: Arc::from(api_key.into()),
            api_base: Arc::from(GEMINI_API_BASE),
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
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

    /// Creates a provider from the configured Google Gemini environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.models.main.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_GOOGLE_API_KEY",
            &config.providers.google.api_key,
        )?;

        let mut provider = Self::new_with_reasoning_effort(
            api_key.clone(),
            default_model,
            config.general.reasoning_effort.clone(),
        )?
        .with_web_search_enabled(config.general.web_search_enabled);
        if let Some(max) = config.providers.google.max_requests_per_min {
            provider = provider.with_rate_limits(PacerConfig::requests_per_min(max));
        }
        provider.limiter = ProviderConcurrency::from_config(config).limiter(
            CallKind::Chat,
            "google",
            &api_key,
            config.providers.google.max_concurrent_requests,
        );
        provider.stream_timeouts = config.providers.stream_timeouts;
        Ok(provider)
    }

    /// Creates a provider from the `MOA_GOOGLE_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("MOA_GOOGLE_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("MOA_GOOGLE_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
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

    /// Overrides request-per-minute pacing for this provider instance.
    #[must_use]
    pub fn with_rate_limits(mut self, config: PacerConfig) -> Self {
        self.pacer = RatePacer::new(config);
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
        let native_tools = enabled_native_tools(
            &model_capabilities,
            self.web_search_enabled
                && request.native_web_search
                    != moa_core::types::completion::NativeWebSearchPolicy::Disabled,
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
        let request_body = match request::build_request_body(
            &request,
            &resolved_model,
            &self.default_reasoning_effort,
            native_tools,
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
        let api_base = Arc::clone(&self.api_base);
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
                let url = format!(
                    "{}/models/{}:streamGenerateContent?alt=sse",
                    api_base.trim_end_matches('/'),
                    resolved_model
                );

                let response =
                    send_with_transport_phase(&span_recorder, &retry_policy, &guard, || {
                        client
                            .post(&url)
                            .header("x-goog-api-key", &*api_key)
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
