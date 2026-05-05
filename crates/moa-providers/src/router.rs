//! Tiered model routing for MOA LLM work.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, ModelTask, Result};

use crate::factory::{
    ProviderSelection, build_provider_from_selection, resolve_provider_selection,
};
use crate::{
    AnthropicProvider, GeminiProvider, OpenAIProvider, models::PROVIDER_ANTHROPIC,
    models::PROVIDER_GOOGLE, models::PROVIDER_OPENAI,
};

/// Routes model calls to the configured main or auxiliary provider instance.
pub struct ModelRouter {
    main: Arc<dyn LLMProvider>,
    auxiliary: Option<Arc<dyn LLMProvider>>,
}

impl ModelRouter {
    /// Creates a router from explicit provider instances.
    #[must_use]
    pub fn new(main: Arc<dyn LLMProvider>, auxiliary: Option<Arc<dyn LLMProvider>>) -> Self {
        Self { main, auxiliary }
    }

    /// Builds a router from the configured main and auxiliary model settings.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        let main_selection =
            resolve_provider_selection(config, Some(config.model_for_task(ModelTask::MainLoop)))?;
        let main_provider = ProviderInstance::from_selection(config, &main_selection)?;
        let auxiliary_selection = config
            .models
            .auxiliary
            .as_deref()
            .map(|model| resolve_provider_selection(config, Some(model)))
            .transpose()?;
        let auxiliary = match auxiliary_selection.as_ref() {
            Some(selection) if selection.provider_name == main_selection.provider_name => {
                Some(main_provider.clone_arc_with_model(selection.model_id.clone())?)
            }
            Some(selection) => Some(build_provider_from_selection(config, selection)?),
            None => None,
        };

        Ok(Self::new(main_provider.into_arc(), auxiliary))
    }

    /// Returns the provider instance that should execute one logical model task.
    #[must_use]
    pub fn provider_for(&self, task: ModelTask) -> Arc<dyn LLMProvider> {
        match task {
            ModelTask::MainLoop => self.main.clone(),
            ModelTask::Summarization
            | ModelTask::Consolidation
            | ModelTask::SkillDistillation
            | ModelTask::Subagent => self.auxiliary.as_ref().unwrap_or(&self.main).clone(),
        }
    }
}

enum ProviderInstance {
    Anthropic(AnthropicProvider),
    OpenAI(OpenAIProvider),
    Gemini(GeminiProvider),
}

impl ProviderInstance {
    fn from_selection(config: &MoaConfig, selection: &ProviderSelection) -> Result<Self> {
        match selection.provider_name.as_str() {
            PROVIDER_ANTHROPIC => Ok(Self::Anthropic(AnthropicProvider::from_config_with_model(
                config,
                selection.model_id.clone(),
            )?)),
            PROVIDER_OPENAI => Ok(Self::OpenAI(OpenAIProvider::from_config_with_model(
                config,
                selection.model_id.clone(),
            )?)),
            PROVIDER_GOOGLE => Ok(Self::Gemini(GeminiProvider::from_config_with_model(
                config,
                selection.model_id.clone(),
            )?)),
            other => {
                unreachable!("validated provider selection contained unsupported provider {other}")
            }
        }
    }

    fn into_arc(self) -> Arc<dyn LLMProvider> {
        match self {
            Self::Anthropic(provider) => Arc::new(provider),
            Self::OpenAI(provider) => Arc::new(provider),
            Self::Gemini(provider) => Arc::new(provider),
        }
    }

    fn clone_arc_with_model(&self, model_id: String) -> Result<Arc<dyn LLMProvider>> {
        match self {
            Self::Anthropic(provider) => Ok(Arc::new(provider.clone_with_model(model_id)?)),
            Self::OpenAI(provider) => Ok(Arc::new(provider.clone_with_model(model_id)?)),
            Self::Gemini(provider) => Ok(Arc::new(provider.clone_with_model(model_id)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        CompletionRequest, CompletionStream, ModelCapabilities, ModelId, ModelTask, ModelTier,
        Result, TokenPricing, ToolCallFormat,
    };

    use super::ModelRouter;

    struct MockProvider {
        name: &'static str,
        capabilities: ModelCapabilities,
    }

    #[async_trait]
    impl moa_core::LLMProvider for MockProvider {
        fn name(&self) -> &str {
            self.name
        }

        fn capabilities(&self) -> ModelCapabilities {
            self.capabilities.clone()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            panic!("mock provider complete() should not be called in router unit tests");
        }
    }

    fn provider(name: &'static str, model: &'static str) -> Arc<dyn moa_core::LLMProvider> {
        Arc::new(MockProvider {
            name,
            capabilities: ModelCapabilities {
                model_id: ModelId::new(model),
                context_window: 200_000,
                max_output: 8_192,
                supports_tools: true,
                supports_vision: false,
                supports_prefix_caching: true,
                cache_ttl: Some(Duration::from_secs(300)),
                tool_call_format: ToolCallFormat::OpenAiCompatible,
                pricing: TokenPricing {
                    input_per_mtok: 1.0,
                    output_per_mtok: 1.0,
                    cached_input_per_mtok: Some(0.1),
                },
                native_tools: Vec::new(),
            },
        })
    }

    #[test]
    fn provider_for_routes_auxiliary_tasks_to_auxiliary_provider() {
        let router = ModelRouter::new(
            provider("anthropic", "claude-sonnet-4-6"),
            Some(provider("anthropic", "claude-haiku-4-5")),
        );

        assert_eq!(
            router
                .provider_for(ModelTask::MainLoop)
                .capabilities()
                .model_id,
            ModelId::new("claude-sonnet-4-6")
        );
        assert_eq!(
            router
                .provider_for(ModelTask::Summarization)
                .capabilities()
                .model_id,
            ModelId::new("claude-haiku-4-5")
        );
        assert_eq!(ModelTask::Summarization.tier(), ModelTier::Auxiliary);
    }

    #[test]
    fn provider_for_falls_back_to_main_when_auxiliary_is_missing() {
        let router = ModelRouter::new(provider("openai", "gpt-5.4"), None);

        assert_eq!(
            router
                .provider_for(ModelTask::MainLoop)
                .capabilities()
                .model_id,
            ModelId::new("gpt-5.4")
        );
        assert_eq!(
            router
                .provider_for(ModelTask::SkillDistillation)
                .capabilities()
                .model_id,
            ModelId::new("gpt-5.4")
        );
    }
}
