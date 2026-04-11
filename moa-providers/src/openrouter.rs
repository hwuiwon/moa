//! OpenRouter provider implementation using the OpenResponses-compatible API.

use std::env;
use std::time::Instant;

use async_openai::config::OpenAIConfig;
use moa_core::{
    CompletionRequest, CompletionStream, LLMProvider, MoaConfig, MoaError, ModelCapabilities,
    Result, TokenPricing, ToolCallFormat,
};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::common::{build_openai_client, build_responses_request, stream_responses_with_retry};
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
    client: async_openai::Client<OpenAIConfig>,
    api_key: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    max_retries: usize,
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
        let config = OpenAIConfig::new()
            .with_api_key(api_key.clone())
            .with_api_base(OPENROUTER_API_BASE);
        let config = config
            .with_header("HTTP-Referer", OPENROUTER_HTTP_REFERER)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .with_header("X-Title", OPENROUTER_TITLE)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        let client = build_openai_client(config)?;

        Ok(Self {
            client,
            api_key,
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            max_retries: DEFAULT_MAX_RETRIES,
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
    }

    /// Creates a provider from the `OPENROUTER_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("OPENROUTER_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("OPENROUTER_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Result<Self> {
        let config = OpenAIConfig::new()
            .with_api_key(self.api_key.clone())
            .with_api_base(api_base.into())
            .with_header("HTTP-Referer", OPENROUTER_HTTP_REFERER)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .with_header("X-Title", OPENROUTER_TITLE)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        self.client = build_openai_client(config)?;
        Ok(self)
    }

    /// Overrides the retry budget for rate-limited requests.
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
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
        let span_recorder = LLMSpanRecorder::new(
            "openrouter",
            resolved_model.clone(),
            &request,
            request.max_output_tokens,
            model_capabilities.pricing.clone(),
        );
        let span = span_recorder.span().clone();
        let request = match build_responses_request(
            &request,
            &resolved_model,
            &self.default_reasoning_effort,
        ) {
            Ok(request) => request,
            Err(error) => {
                span_recorder.fail(&error);
                return Err(error);
            }
        };
        let client = self.client.clone();
        let max_retries = self.max_retries;
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);

        let completion_task = tokio::spawn(
            async move {
                let started_at = Instant::now();
                stream_responses_with_retry(
                    &client,
                    &request,
                    tx,
                    resolved_model,
                    started_at,
                    max_retries,
                    span_recorder,
                )
                .await
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
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_model_id, capabilities_for_model};

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

        let anthropic_capabilities = capabilities_for_model("anthropic/claude-opus-4-6");
        assert_eq!(anthropic_capabilities.context_window, 1_000_000);
        assert_eq!(anthropic_capabilities.max_output, 128_000);
    }
}
