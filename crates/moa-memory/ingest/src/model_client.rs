//! Provider-backed text generation for memory ingestion.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_config::MemoryExtractionConfig;
use moa_config::MoaConfig;
use moa_core::{
    traits::LLMProvider, types::completion::CompletionRequest,
    types::completion::CompletionResponse, types::context::ContextMessage,
    types::identifiers::ModelId,
};
use moa_providers::build_provider_from_model;
use serde_json::json;
use tokio::time::timeout;

use crate::{Error, Result};

/// Observer for one provider-backed memory-model call.
///
/// Evaluation lanes use this seam to reserve budget before provider execution
/// and record normalized provider usage after a successful response. Production
/// callers that do not install an observer retain the existing behavior.
#[async_trait]
pub trait ModelCallObserver: Send + Sync {
    /// Runs after request construction and before the provider is invoked.
    async fn before_call(&self, request: &CompletionRequest) -> Result<()>;

    /// Records the aggregated provider response and its normalized usage.
    async fn after_response(&self, response: &CompletionResponse) -> Result<()>;

    /// Records that the provider call failed or timed out after reservation.
    async fn after_failure(&self) {}
}

/// Shared model client used by memory extraction, entity merge, and judging.
#[derive(Clone)]
pub(crate) struct ModelTextClient {
    provider: Arc<dyn LLMProvider>,
    model: ModelId,
    timeout: Duration,
    observer: Option<Arc<dyn ModelCallObserver>>,
}

impl ModelTextClient {
    /// Creates a memory model client from explicit provider wiring.
    pub(crate) fn new(
        provider: Arc<dyn LLMProvider>,
        model: ModelId,
        timeout_ms: u64,
    ) -> Result<Self> {
        if timeout_ms == 0 {
            return Err(Error::ModelInference(
                "memory.extraction.timeout_ms must be greater than zero".to_string(),
            ));
        }
        Ok(Self {
            provider,
            model,
            timeout: Duration::from_millis(timeout_ms),
            observer: None,
        })
    }

    /// Creates a memory model client with a pre/post provider-call observer.
    pub(crate) fn new_with_observer(
        provider: Arc<dyn LLMProvider>,
        model: ModelId,
        timeout_ms: u64,
        observer: Arc<dyn ModelCallObserver>,
    ) -> Result<Self> {
        let mut client = Self::new(provider, model, timeout_ms)?;
        client.observer = Some(observer);
        Ok(client)
    }

    /// Builds a memory model client through the shared provider registry.
    pub(crate) fn from_config(
        config: &MoaConfig,
        extraction: &MemoryExtractionConfig,
    ) -> Result<Self> {
        let model = extraction.model.trim();
        if model.is_empty() {
            return Err(Error::ModelInference(
                "memory.extraction.model is required".to_string(),
            ));
        }
        let (provider, model_id) = build_provider_from_model(config, Some(model))
            .map_err(|error| Error::ModelInference(error.to_string()))?;
        Self::new(provider, model_id, extraction.timeout_ms)
    }

    /// Builds a configured memory model client with an explicit observer.
    pub(crate) fn from_config_with_observer(
        config: &MoaConfig,
        extraction: &MemoryExtractionConfig,
        observer: Arc<dyn ModelCallObserver>,
    ) -> Result<Self> {
        let model = extraction.model.trim();
        if model.is_empty() {
            return Err(Error::ModelInference(
                "memory.extraction.model is required".to_string(),
            ));
        }
        let (provider, model_id) = build_provider_from_model(config, Some(model))
            .map_err(|error| Error::ModelInference(error.to_string()))?;
        Self::new_with_observer(provider, model_id, extraction.timeout_ms, observer)
    }

    /// Returns the model id this client sends with each completion request.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn model_id(&self) -> &ModelId {
        &self.model
    }

    /// Sends one system/user prompt pair and returns the aggregated assistant text.
    pub(crate) async fn complete_text(&self, system: &str, user: &str) -> Result<String> {
        let mut messages = Vec::with_capacity(2);
        if !system.trim().is_empty() {
            messages.push(ContextMessage::system(system));
        }
        messages.push(ContextMessage::user(user));

        let mut request = CompletionRequest {
            model: Some(self.model.clone()),
            messages,
            tools: Vec::new(),
            max_output_tokens: Some(2_048),
            temperature: Some(0.0),
            response_format: None,
            native_web_search: Default::default(),
            metadata: Default::default(),
        };
        request
            .metadata
            .insert("moa.memory.task".to_string(), json!("ingestion"));

        if let Some(observer) = &self.observer {
            observer.before_call(&request).await?;
        }

        let response = match timeout(self.timeout, async {
            let stream = self
                .provider
                .complete(request)
                .await
                .map_err(|error| Error::ModelInference(error.to_string()))?;
            stream
                .collect()
                .await
                .map_err(|error| Error::ModelInference(error.to_string()))
        })
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                if let Some(observer) = &self.observer {
                    observer.after_failure().await;
                }
                return Err(error);
            }
            Err(_) => {
                if let Some(observer) = &self.observer {
                    observer.after_failure().await;
                }
                return Err(Error::ModelInference(format!(
                    "memory model request timed out after {} ms",
                    self.timeout.as_millis()
                )));
            }
        };

        if let Some(observer) = &self.observer {
            observer.after_response(&response).await?;
        }

        if response.text.trim().is_empty() {
            return Err(Error::ModelInference(
                "memory model response did not contain assistant text".to_string(),
            ));
        }
        Ok(response.text)
    }
}

/// Returns enabled memory extraction config.
#[must_use]
pub(crate) fn resolved_extraction_config(config: &MoaConfig) -> Option<MemoryExtractionConfig> {
    config
        .memory
        .extraction
        .enabled
        .then(|| config.memory.extraction.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use moa_core::{
        types::completion::CompletionResponse, types::completion::CompletionStream,
        types::completion::StopReason, types::completion::TokenUsage,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
    };

    use super::*;

    #[derive(Default)]
    struct CapturingProvider {
        request: Mutex<Option<CompletionRequest>>,
        response: String,
        usage: TokenUsage,
    }

    #[async_trait]
    impl LLMProvider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                model_id: ModelId::new("gpt-5.4-mini"),
                context_window: 400_000,
                max_output: 128_000,
                supports_tools: true,
                supports_vision: true,
                supports_prefix_caching: true,
                cache_ttl: None,
                tool_call_format: ToolCallFormat::OpenAiCompatible,
                pricing: TokenPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cached_input_per_mtok: None,
                    cache_write_5m_per_mtok: None,
                    cache_write_1h_per_mtok: None,
                },
                native_tools: Vec::new(),
            }
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> moa_core::error::Result<CompletionStream> {
            *self.request.lock().expect("capture request") = Some(request);
            Ok(CompletionStream::from_response(CompletionResponse {
                text: self.response.clone(),
                content: Vec::new(),
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("gpt-5.4-mini"),
                usage: self.usage,
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    #[test]
    fn resolved_extraction_config_does_not_require_cohere_key() {
        // Pins: enabling model-backed memory extraction no longer depends on a
        // nested memory key or Cohere credential fan-in.
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;
        config.providers.openai.api_key = "test-openai-key".to_string();

        let extraction =
            resolved_extraction_config(&config).expect("enabled extraction should resolve");

        assert_eq!(extraction.model, "gpt-5.4-mini");
    }

    #[test]
    fn model_text_client_from_config_uses_fast_openai_default() {
        // Pins: memory extraction defaults to the shared provider registry's
        // small fast model instead of Cohere chat.
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;
        config.providers.openai.api_key = "test-openai-key".to_string();

        let extraction = resolved_extraction_config(&config).expect("enabled extraction");
        let client = ModelTextClient::from_config(&config, &extraction)
            .expect("configured OpenAI provider should build");

        assert_eq!(client.model_id().as_str(), "gpt-5.4-mini");
    }

    #[test]
    fn model_text_client_rejects_zero_timeout() {
        // Pins: invalid memory model timeout fails during setup, not inside a turn.
        let provider = Arc::new(CapturingProvider {
            request: Mutex::new(None),
            response: "ok".to_string(),
            usage: TokenUsage::default(),
        });

        let error = match ModelTextClient::new(provider, ModelId::new("gpt-5.4-mini"), 0) {
            Ok(_) => panic!("zero timeout should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("timeout_ms"));
    }

    #[tokio::test]
    async fn model_text_client_sends_system_user_messages_and_model() {
        // Pins: memory callers use the standard LLMProvider request shape with
        // model id, system prompt, user prompt, and deterministic temperature.
        let provider = Arc::new(CapturingProvider {
            request: Mutex::new(None),
            response: "assistant text".to_string(),
            usage: TokenUsage::default(),
        });
        let client = ModelTextClient::new(provider.clone(), ModelId::new("gpt-5.4-mini"), 1_000)
            .expect("client should build");

        let response = client
            .complete_text("system prompt", "user prompt")
            .await
            .expect("completion should succeed");

        assert_eq!(response, "assistant text");
        let request = provider
            .request
            .lock()
            .expect("capture request")
            .clone()
            .expect("request captured");
        assert_eq!(request.model, Some(ModelId::new("gpt-5.4-mini")));
        assert_eq!(request.messages[0], ContextMessage::system("system prompt"));
        assert_eq!(request.messages[1], ContextMessage::user("user prompt"));
        assert_eq!(request.temperature, Some(0.0));
    }

    #[derive(Default)]
    struct CapturingObserver {
        events: Mutex<Vec<&'static str>>,
        usage: Mutex<Option<TokenUsage>>,
    }

    #[async_trait]
    impl ModelCallObserver for CapturingObserver {
        async fn before_call(&self, _request: &CompletionRequest) -> Result<()> {
            self.events.lock().expect("observer events").push("before");
            Ok(())
        }

        async fn after_response(&self, response: &CompletionResponse) -> Result<()> {
            self.events.lock().expect("observer events").push("after");
            *self.usage.lock().expect("observer usage") = Some(response.usage);
            Ok(())
        }

        async fn after_failure(&self) {
            self.events.lock().expect("observer events").push("failure");
        }
    }

    #[tokio::test]
    async fn model_text_client_observes_forecast_boundary_and_provider_usage() {
        // Pins: benchmark accounting can reserve budget before the real memory-model call and
        // receive the exact normalized provider usage after its response.
        let expected_usage = TokenUsage {
            input_tokens_uncached: 17,
            input_tokens_cache_write: 3,
            input_tokens_cache_read: 5,
            output_tokens: 11,
        };
        let provider = Arc::new(CapturingProvider {
            request: Mutex::new(None),
            response: "assistant text".to_string(),
            usage: expected_usage,
        });
        let observer = Arc::new(CapturingObserver::default());
        let client = ModelTextClient::new_with_observer(
            provider,
            ModelId::new("gpt-5.4-mini"),
            1_000,
            observer.clone(),
        )
        .expect("observed client should build");

        client
            .complete_text("system prompt", "user prompt")
            .await
            .expect("observed completion should succeed");

        assert_eq!(
            *observer.events.lock().expect("observer events"),
            vec!["before", "after"]
        );
        assert_eq!(
            *observer.usage.lock().expect("observer usage"),
            Some(expected_usage)
        );
    }
}
