//! Tiered model routing for MOA LLM work.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::{error::Result, traits::LLMProvider, types::provider::ModelTask};

use crate::ProviderRegistry;

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
        ProviderRegistry::from_config(config).model_router_for_config(config)
    }

    /// Returns the provider instance that should execute one logical model task.
    #[must_use]
    pub fn provider_for(&self, task: ModelTask) -> Arc<dyn LLMProvider> {
        match task {
            ModelTask::MainLoop => self.main.clone(),
            ModelTask::Summarization
            | ModelTask::Consolidation
            | ModelTask::SkillDistillation
            | ModelTask::Worker => self.auxiliary.as_ref().unwrap_or(&self.main).clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        error::Result, types::completion::CompletionRequest, types::completion::CompletionStream,
        types::identifiers::ModelId, types::model::ModelCapabilities, types::model::TokenPricing,
        types::model::ToolCallFormat, types::provider::ModelTask, types::provider::ModelTier,
    };

    use super::ModelRouter;

    struct MockProvider {
        name: &'static str,
        capabilities: ModelCapabilities,
    }

    #[async_trait]
    impl moa_core::traits::LLMProvider for MockProvider {
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

    fn provider(name: &'static str, model: &'static str) -> Arc<dyn moa_core::traits::LLMProvider> {
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
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
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
