//! Unit coverage for the Restate LLM gateway provider dispatch and buffering helpers.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, LLMProvider,
    MoaError, StopReason, TokenPricing, TokenUsage, ToolCallFormat,
};
use moa_orchestrator::services::llm_gateway::{
    LLMGatewayImpl, ProviderKind, ProviderRegistry, compute_cost_cents,
};

#[derive(Clone)]
struct MockProvider {
    name: &'static str,
    model: &'static str,
    pricing: TokenPricing,
    response: MockOutcome,
    requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl MockProvider {
    fn success(name: &'static str, model: &'static str, pricing: TokenPricing) -> Self {
        Self {
            name,
            model,
            pricing,
            response: MockOutcome::Success(CompletionResponse {
                text: "ok".to_string(),
                content: vec![CompletionContent::Text("ok".to_string())],
                stop_reason: StopReason::EndTurn,
                model: model.into(),
                usage: TokenUsage {
                    input_tokens_uncached: 48,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 16,
                    output_tokens: 32,
                },
                duration_ms: 7,
                thought_signature: None,
            }),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn error(name: &'static str, model: &'static str, pricing: TokenPricing, error: &str) -> Self {
        Self {
            name,
            model,
            pricing,
            response: MockOutcome::Error(error.to_string()),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn recorded_requests(&self) -> Vec<CompletionRequest> {
        self.requests
            .lock()
            .expect("mock provider request log should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl LLMProvider for MockProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn capabilities(&self) -> moa_core::ModelCapabilities {
        moa_core::ModelCapabilities {
            model_id: self.model.into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: self.pricing.clone(),
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: CompletionRequest) -> moa_core::Result<CompletionStream> {
        self.requests
            .lock()
            .expect("mock provider request log should not be poisoned")
            .push(request);

        match &self.response {
            MockOutcome::Success(response) => Ok(CompletionStream::from_response(response.clone())),
            MockOutcome::Error(message) => Err(MoaError::ProviderError(message.clone())),
        }
    }
}

#[derive(Clone)]
enum MockOutcome {
    Success(CompletionResponse),
    Error(String),
}

#[test]
fn llm_gateway_resolve_provider_for_claude_model() {
    let registry = ProviderRegistry::with_static_providers(
        Some(Arc::new(MockProvider::success(
            "anthropic",
            "claude-sonnet-4-6",
            pricing(3.0, 15.0, Some(0.3)),
        ))),
        None,
        None,
    );

    let (provider_kind, model_id) = registry
        .resolve_provider_kind(Some("claude-sonnet-4-6"))
        .expect("claude model should resolve");

    assert_eq!(provider_kind, ProviderKind::Anthropic);
    assert_eq!(model_id.as_str(), "claude-sonnet-4-6");
}

#[test]
fn llm_gateway_resolve_provider_for_gpt_model() {
    let registry = ProviderRegistry::with_static_providers(
        None,
        Some(Arc::new(MockProvider::success(
            "openai",
            "gpt-5.4",
            pricing(2.5, 15.0, Some(0.25)),
        ))),
        None,
    );

    let (provider_kind, model_id) = registry
        .resolve_provider_kind(Some("gpt-5.4"))
        .expect("gpt model should resolve");

    assert_eq!(provider_kind, ProviderKind::OpenAI);
    assert_eq!(model_id.as_str(), "gpt-5.4");
}

#[test]
fn llm_gateway_resolve_provider_for_prefixed_google_model() {
    let registry = ProviderRegistry::with_static_providers(
        None,
        None,
        Some(Arc::new(MockProvider::success(
            "google",
            "gemini-2.5-flash",
            pricing(0.3, 2.5, Some(0.03)),
        ))),
    );

    let (provider_kind, model_id) = registry
        .resolve_provider_kind(Some("google:gemini-2.5-flash"))
        .expect("prefixed google model should resolve");

    assert_eq!(provider_kind, ProviderKind::Google);
    assert_eq!(model_id.as_str(), "gemini-2.5-flash");
}

#[test]
fn llm_gateway_compute_cost_cents_sonnet() {
    let cents = compute_cost_cents(
        "claude-sonnet-4-6",
        TokenUsage {
            input_tokens_uncached: 100_000,
            input_tokens_cache_write: 25_000,
            input_tokens_cache_read: 50_000,
            output_tokens: 20_000,
        },
    );

    assert_eq!(cents, 69);
}

#[tokio::test]
async fn llm_gateway_complete_propagates_provider_error() {
    let registry = ProviderRegistry::with_static_providers(
        None,
        Some(Arc::new(MockProvider::error(
            "openai",
            "gpt-5.4",
            pricing(2.5, 15.0, Some(0.25)),
            "provider boom",
        ))),
        None,
    );
    let gateway = LLMGatewayImpl::new(Arc::new(registry));

    let error = gateway
        .complete_buffered(CompletionRequest::simple("hello").with_model("gpt-5.4"))
        .await
        .expect_err("provider failures should bubble out of the gateway");

    assert!(
        error.to_string().contains("provider boom"),
        "expected provider error to be preserved, got {error}"
    );
}

#[tokio::test]
async fn llm_gateway_complete_normalizes_explicit_provider_prefix() {
    let provider =
        MockProvider::success("google", "gemini-2.5-flash", pricing(0.3, 2.5, Some(0.03)));
    let registry =
        ProviderRegistry::with_static_providers(None, None, Some(Arc::new(provider.clone())));
    let gateway = LLMGatewayImpl::new(Arc::new(registry));

    let response = gateway
        .complete_buffered(CompletionRequest::simple("hello").with_model("google:gemini-2.5-flash"))
        .await
        .expect("prefixed provider request should complete");

    let recorded = provider.recorded_requests();
    assert_eq!(response.model.as_str(), "gemini-2.5-flash");
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0]
            .model
            .as_ref()
            .expect("gateway should normalize an explicit provider prefix")
            .as_str(),
        "gemini-2.5-flash"
    );
}

fn pricing(
    input_per_mtok: f64,
    output_per_mtok: f64,
    cached_input_per_mtok: Option<f64>,
) -> TokenPricing {
    TokenPricing {
        input_per_mtok,
        output_per_mtok,
        cached_input_per_mtok,
    }
}

trait CompletionRequestExt {
    fn with_model(self, model: &str) -> CompletionRequest;
}

impl CompletionRequestExt for CompletionRequest {
    fn with_model(mut self, model: &str) -> CompletionRequest {
        self.model = Some(model.into());
        self
    }
}
