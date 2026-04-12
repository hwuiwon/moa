//! OpenAI Responses API provider implementation.

use std::env;
use std::time::Instant;

use async_openai::config::OpenAIConfig;
use moa_core::{
    CompletionRequest, CompletionStream, LLMProvider, MoaConfig, MoaError, ModelCapabilities,
    ProviderNativeTool, Result, TokenPricing, ToolCallFormat,
};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::common::{build_openai_client, build_responses_request, stream_responses_with_retry};
use crate::instrumentation::LLMSpanRecorder;

const DEFAULT_STREAM_BUFFER: usize = 128;
const DEFAULT_MAX_RETRIES: usize = 3;
const MODEL_GPT_5_4: &str = "gpt-5.4";
const MODEL_GPT_5_4_MINI: &str = "gpt-5.4-mini";
const MODEL_GPT_5_4_NANO: &str = "gpt-5.4-nano";
const MODEL_GPT_5_MINI: &str = "gpt-5-mini";
const MODEL_GPT_5_NANO: &str = "gpt-5-nano";

/// OpenAI provider backed by the Responses API.
pub struct OpenAIProvider {
    client: async_openai::Client<OpenAIConfig>,
    api_key: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    max_retries: usize,
    web_search_enabled: bool,
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
        let client = build_openai_client(OpenAIConfig::new().with_api_key(api_key.clone()))?;

        Ok(Self {
            client,
            api_key,
            default_model,
            default_reasoning_effort: default_reasoning_effort.into(),
            default_capabilities,
            max_retries: DEFAULT_MAX_RETRIES,
            web_search_enabled: true,
        })
    }

    /// Creates a provider from the configured OpenAI environment variable.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::from_config_with_model(config, config.general.default_model.clone())
    }

    /// Creates a provider from config with an explicit default model override.
    pub fn from_config_with_model(
        config: &MoaConfig,
        default_model: impl Into<String>,
    ) -> Result<Self> {
        let api_key_env = config.providers.openai.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;

        Self::new_with_reasoning_effort(
            api_key,
            default_model,
            config.general.reasoning_effort.clone(),
        )
        .map(|provider| provider.with_web_search_enabled(config.general.web_search_enabled))
    }

    /// Creates a provider from the `OPENAI_API_KEY` environment variable.
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let api_key = env::var("OPENAI_API_KEY")
            .map_err(|_| MoaError::MissingEnvironmentVariable("OPENAI_API_KEY".to_string()))?;

        Self::new(api_key, default_model)
    }

    /// Overrides the Responses API base URL, primarily for tests.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Result<Self> {
        let config = OpenAIConfig::new()
            .with_api_key(self.api_key.clone())
            .with_api_base(api_base.into());
        self.client = build_openai_client(config)?;
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
impl LLMProvider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
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
        let model_capabilities = capabilities_for_model(&resolved_model)?;
        let span_recorder = LLMSpanRecorder::new(
            "openai",
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
            native_tools(&model_capabilities, self.web_search_enabled),
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

pub(crate) fn canonical_model_id(model: &str) -> Result<String> {
    if model.starts_with(MODEL_GPT_5_4_MINI) {
        return Ok(model.to_string());
    }
    if model.starts_with(MODEL_GPT_5_4_NANO) {
        return Ok(model.to_string());
    }
    if model.starts_with(MODEL_GPT_5_4) {
        return Ok(model.to_string());
    }
    if model.starts_with(MODEL_GPT_5_MINI) {
        return Ok(model.to_string());
    }
    if model.starts_with(MODEL_GPT_5_NANO) {
        return Ok(model.to_string());
    }

    Err(MoaError::Unsupported(format!(
        "unsupported OpenAI model '{model}'"
    )))
}

pub(crate) fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    if model.starts_with(MODEL_GPT_5_4_MINI) {
        return Ok(ModelCapabilities {
            model_id: model.to_string(),
            context_window: 400_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 0.75,
                output_per_mtok: 4.50,
                cached_input_per_mtok: Some(0.075),
            },
            native_tools: native_web_search_tools(),
        });
    }

    if model.starts_with(MODEL_GPT_5_4_NANO) {
        return Ok(ModelCapabilities {
            model_id: model.to_string(),
            context_window: 400_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 0.20,
                output_per_mtok: 1.25,
                cached_input_per_mtok: Some(0.02),
            },
            native_tools: native_web_search_tools(),
        });
    }

    if model.starts_with(MODEL_GPT_5_4) {
        return Ok(ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_050_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 2.50,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.25),
            },
            native_tools: native_web_search_tools(),
        });
    }

    if model.starts_with(MODEL_GPT_5_MINI) {
        return Ok(ModelCapabilities {
            model_id: model.to_string(),
            context_window: 400_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 0.25,
                output_per_mtok: 2.0,
                cached_input_per_mtok: Some(0.025),
            },
            native_tools: native_web_search_tools(),
        });
    }

    if model.starts_with(MODEL_GPT_5_NANO) {
        return Ok(ModelCapabilities {
            model_id: model.to_string(),
            context_window: 400_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 0.05,
                output_per_mtok: 0.40,
                cached_input_per_mtok: Some(0.005),
            },
            native_tools: native_web_search_tools(),
        });
    }

    Err(MoaError::Unsupported(format!(
        "unsupported OpenAI model '{model}'"
    )))
}

fn native_tools(capabilities: &ModelCapabilities, enabled: bool) -> &[ProviderNativeTool] {
    if enabled {
        &capabilities.native_tools
    } else {
        &[]
    }
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
