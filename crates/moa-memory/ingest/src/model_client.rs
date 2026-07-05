//! Provider-backed text generation for memory ingestion.

use std::sync::Arc;
use std::time::Duration;

use moa_core::config::MemoryExtractionConfig;
use moa_core::{CompletionRequest, ContextMessage, LLMProvider, MoaConfig, ModelId};
use moa_providers::build_provider_from_model;
use serde_json::json;
use tokio::time::timeout;

use crate::{IngestError, Result};

/// Shared model client used by memory extraction, entity merge, and judging.
#[derive(Clone)]
pub(crate) struct ModelTextClient {
    provider: Arc<dyn LLMProvider>,
    model: ModelId,
    timeout: Duration,
}

impl ModelTextClient {
    /// Creates a memory model client from explicit provider wiring.
    pub(crate) fn new(
        provider: Arc<dyn LLMProvider>,
        model: ModelId,
        timeout_ms: u64,
    ) -> Result<Self> {
        if timeout_ms == 0 {
            return Err(IngestError::ModelInference(
                "memory.extraction.timeout_ms must be greater than zero".to_string(),
            ));
        }
        Ok(Self {
            provider,
            model,
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    /// Builds a memory model client through the shared provider registry.
    pub(crate) fn from_config(
        config: &MoaConfig,
        extraction: &MemoryExtractionConfig,
    ) -> Result<Self> {
        let model = extraction.model.trim();
        if model.is_empty() {
            return Err(IngestError::ModelInference(
                "memory.extraction.model is required".to_string(),
            ));
        }
        let (provider, model_id) = build_provider_from_model(config, Some(model))
            .map_err(|error| IngestError::ModelInference(error.to_string()))?;
        Self::new(provider, model_id, extraction.timeout_ms)
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
            metadata: Default::default(),
        };
        request
            .metadata
            .insert("moa.memory.task".to_string(), json!("ingestion"));

        let response = timeout(self.timeout, async {
            let stream = self
                .provider
                .complete(request)
                .await
                .map_err(|error| IngestError::ModelInference(error.to_string()))?;
            stream
                .collect()
                .await
                .map_err(|error| IngestError::ModelInference(error.to_string()))
        })
        .await
        .map_err(|_| {
            IngestError::ModelInference(format!(
                "memory model request timed out after {} ms",
                self.timeout.as_millis()
            ))
        })??;

        if response.text.trim().is_empty() {
            return Err(IngestError::ModelInference(
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
        CompletionResponse, CompletionStream, ModelCapabilities, StopReason, TokenPricing,
        TokenUsage, ToolCallFormat,
    };

    use super::*;

    #[derive(Default)]
    struct CapturingProvider {
        request: Mutex<Option<CompletionRequest>>,
        response: String,
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

        async fn complete(&self, request: CompletionRequest) -> moa_core::Result<CompletionStream> {
            *self.request.lock().expect("capture request") = Some(request);
            Ok(CompletionStream::from_response(CompletionResponse {
                text: self.response.clone(),
                content: Vec::new(),
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("gpt-5.4-mini"),
                usage: TokenUsage::default(),
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
}
