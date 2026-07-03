//! `OpenAI` Responses API provider implementation.

use std::env;
use std::time::Instant;

use async_openai::config::OpenAIConfig;
use moa_core::{
    CompletionRequest, CompletionStream, LLMProvider, MoaConfig, MoaError, ModelCapabilities,
    ModelId, ProviderNativeTool, Result,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::adapters::openai_responses::{
    build_openai_client, build_responses_request, stream_responses_with_retry,
};
use crate::core::concurrency::{ConcurrencyLimiter, DEFAULT_BLOCK_THRESHOLD};
use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::models::{self, PROVIDER_OPENAI};
use crate::core::pacer::{PacerConfig, RatePacer};
use crate::core::provider_tools::enabled_native_tools;
use crate::core::rate_guard::{self, RateGuard};
use crate::core::retry::RetryPolicy;

const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_RETRIES: usize = 3;
const MODEL_GPT_5_4: &str = "gpt-5.4";
#[cfg(test)]
const MODEL_GPT_5_4_MINI: &str = "gpt-5.4-mini";

/// Builds an OpenAI Responses request body for inspection tests without sending it.
pub fn debug_build_openai_request_body(
    request: &CompletionRequest,
    web_search_enabled: bool,
) -> Result<Value> {
    let requested_model = request
        .model
        .as_ref()
        .map(ModelId::as_str)
        .unwrap_or(MODEL_GPT_5_4);
    let resolved_model = canonical_model_id(requested_model)?;
    let capabilities = capabilities_for_model(&resolved_model)?;
    let request = build_responses_request(
        request,
        &resolved_model,
        "medium",
        enabled_native_tools(&capabilities, web_search_enabled),
    )?;
    serde_json::to_value(request).map_err(MoaError::from)
}

/// `OpenAI` provider backed by the Responses API.
pub struct OpenAIProvider {
    client: async_openai::Client<OpenAIConfig>,
    api_key: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
    pacer: RatePacer,
    limiter: ConcurrencyLimiter,
    guard: RateGuard,
}

impl OpenAIProvider {
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
        let api_key = api_key.into();
        let client = build_openai_client(OpenAIConfig::new().with_api_key(api_key.clone()));

        Ok(Self {
            client,
            api_key,
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            retry_policy: RetryPolicy::default().with_max_retries(DEFAULT_MAX_RETRIES),
            web_search_enabled: true,
            pacer: RatePacer::new(PacerConfig::disabled()),
            // LLM concurrency is unbounded by default; operators opt in per key.
            limiter: ConcurrencyLimiter::unbounded(),
            guard: RateGuard::new(),
        })
    }

    /// Creates a provider from the configured `OpenAI` environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.models.main.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key = moa_core::config::required_config_secret(
            "MOA_OPENAI_API_KEY",
            &config.providers.openai.api_key,
        )?;

        let mut provider = Self::new_with_reasoning_effort(
            api_key,
            default_model,
            config.general.reasoning_effort.clone(),
        )?
        .with_web_search_enabled(config.general.web_search_enabled);
        if let Some(max) = config.providers.openai.max_requests_per_min {
            provider = provider.with_rate_limits(PacerConfig::requests_per_min(max));
        }
        if let Some(max) = config.providers.openai.max_concurrent_requests {
            provider = provider.with_max_concurrent_requests(max as usize);
        }
        Ok(provider)
    }

    /// Creates a provider from the `MOA_OPENAI_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("MOA_OPENAI_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("MOA_OPENAI_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the Responses API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Result<Self> {
        let config = OpenAIConfig::new()
            .with_api_key(self.api_key.clone())
            .with_api_base(api_base.into());
        self.client = build_openai_client(config);
        Ok(self)
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
impl LLMProvider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
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
        let span_recorder = LLMSpanRecorder::new(
            "openai",
            resolved_model.clone(),
            &request,
            request.max_output_tokens,
            model_capabilities.pricing.clone(),
        );
        span_recorder.set_phase("build_request");
        let span = span_recorder.span().clone();
        let request = match build_responses_request(
            &request,
            &resolved_model,
            &self.default_reasoning_effort,
            enabled_native_tools(&model_capabilities, self.web_search_enabled),
        ) {
            Ok(request) => request,
            Err(error) => {
                span_recorder.fail_at_stage("build_request", &error);
                return Err(error);
            }
        };
        // Take an in-flight slot before dispatching; a gate that stays saturated
        // past the block threshold is a failover-eligible block, not a queue.
        let permit = match self.limiter.acquire_within(DEFAULT_BLOCK_THRESHOLD).await {
            Some(lease) => lease,
            None => {
                let error = rate_guard::rate_limited_saturated(DEFAULT_BLOCK_THRESHOLD);
                span_recorder.fail_at_stage("transport", &error);
                return Err(error);
            }
        };
        // Chat completions are request-rate limited; pace after taking the
        // in-flight slot so queued callers do not consume rate budget early.
        self.pacer.acquire(1, 0).await;
        let client = self.client.clone();
        let retry_policy = self.retry_policy.clone();
        let guard = self.guard.clone();
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                // Hold the in-flight slot for the whole generation; released when
                // the streamed completion finishes.
                let _permit = permit;
                let started_at = Instant::now();
                stream_responses_with_retry(
                    &client,
                    &request,
                    tx,
                    ModelId::new(resolved_model),
                    started_at,
                    retry_policy,
                    &guard,
                    span_recorder,
                )
                .await
            }
            .instrument(span),
        );

        Ok(CompletionStream::new(rx, completion_task))
    }
}

pub(crate) fn canonical_model_id(model: &str) -> Result<String> {
    models::canonical_model_id(PROVIDER_OPENAI, "OpenAI", model)
}

pub(crate) fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    models::capabilities_for_provider_model(PROVIDER_OPENAI, model, native_web_search_tools())
}

fn native_web_search_tools() -> Vec<ProviderNativeTool> {
    vec![ProviderNativeTool {
        tool_type: "web_search".to_string(),
        name: "web_search".to_string(),
        config: None,
    }]
}

#[cfg(test)]
mod tests {
    use moa_core::ToolCallFormat;

    use super::{MODEL_GPT_5_4, MODEL_GPT_5_4_MINI, canonical_model_id, capabilities_for_model};

    #[test]
    fn gpt_5_4_family_reports_expected_capabilities() {
        let gpt_5_4 = capabilities_for_model(MODEL_GPT_5_4).unwrap();
        assert_eq!(gpt_5_4.context_window, 1_050_000);
        assert_eq!(gpt_5_4.max_output, 128_000);
        assert!(gpt_5_4.supports_tools);
        assert!(gpt_5_4.supports_prefix_caching);
        assert_eq!(gpt_5_4.tool_call_format, ToolCallFormat::OpenAiCompatible);

        let gpt_5_4_mini = capabilities_for_model(MODEL_GPT_5_4_MINI).unwrap();
        assert_eq!(gpt_5_4_mini.context_window, 400_000);
        assert_eq!(gpt_5_4_mini.max_output, 128_000);
        assert!(gpt_5_4_mini.supports_tools);
        assert!(gpt_5_4_mini.supports_prefix_caching);
    }

    #[test]
    fn unsupported_models_are_rejected() {
        assert!(canonical_model_id("gpt-4.1").is_err());
        assert!(capabilities_for_model("gpt-4.1").is_err());
    }
}
