//! Unit coverage for the Restate LLM gateway provider dispatch and buffering helpers.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moa_core::{
    error::MoaError, traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::CompletionStream, types::completion::DEFER_BRAIN_RESPONSE_METADATA_KEY,
    types::completion::StopReason, types::completion::TokenUsage, types::identifiers::SessionId,
    types::model::TokenPricing, types::model::ToolCallFormat,
};
use moa_orchestrator::services::llm_gateway::{
    LLMGatewayImpl, compute_cost_cents, should_defer_brain_response,
};
use moa_providers::ProviderRegistry;
use moa_test_support::pricing::PricingTable;
use serde_json::json;

const CENTS_PER_DOLLAR: f64 = 100.0;

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

    fn capabilities(&self) -> moa_core::types::model::ModelCapabilities {
        moa_core::types::model::ModelCapabilities {
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

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
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
fn llm_gateway_compute_cost_cents_prices_sonnet_cache_writes_at_creation_rate() {
    // Pins: Anthropic cache-write tokens are not billed at the base input rate.
    let model = "claude-sonnet-4-6";
    let usage = TokenUsage {
        input_tokens_uncached: 100_000,
        input_tokens_cache_write: 25_000,
        input_tokens_cache_read: 50_000,
        output_tokens: 20_000,
    };

    assert_eq!(compute_cost_cents(model, usage), 71);
}

#[tokio::test]
async fn llm_gateway_complete_propagates_provider_error() {
    let registry = ProviderRegistry::with_static_providers(
        None,
        Some(Arc::new(MockProvider::error(
            "openai",
            "gpt-5.4",
            token_pricing_from_fixture("openai", "gpt-4.1"),
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
    let provider = MockProvider::success(
        "google",
        "gemini-3-flash-preview",
        token_pricing_from_fixture("gemini", "gemini-3-flash-preview"),
    );
    let registry =
        ProviderRegistry::with_static_providers(None, None, Some(Arc::new(provider.clone())));
    let gateway = LLMGatewayImpl::new(Arc::new(registry));

    let response = gateway
        .complete_buffered(
            CompletionRequest::simple("hello").with_model("google:gemini-3-flash-preview"),
        )
        .await
        .expect("prefixed provider request should complete");

    let recorded = provider.recorded_requests();
    assert_eq!(response.model.as_str(), "gemini-3-flash-preview");
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0]
            .model
            .as_ref()
            .expect("gateway should normalize an explicit provider prefix")
            .as_str(),
        "gemini-3-flash-preview"
    );
}

#[test]
fn llm_gateway_guardrail_direct_session_request_without_defer_keeps_persistence_enabled() {
    // Pins: direct LLMGateway callers with only session metadata keep the BrainResponse write path.
    let request = request_with_session_metadata(false);

    assert!(!should_defer_brain_response(&request));
}

#[test]
fn llm_gateway_guardrail_direct_session_request_with_defer_skips_gateway_persistence() {
    // Pins: TurnExecution can buffer output for guardrails by setting the explicit defer flag.
    let request = request_with_session_metadata(true);

    assert!(should_defer_brain_response(&request));
}

fn token_pricing_from_fixture(provider: &str, model: &str) -> TokenPricing {
    let table = PricingTable::load();
    let pricing = table
        .get(provider, model)
        .unwrap_or_else(|error| panic!("missing fixture pricing for {provider}/{model}: {error}"));
    TokenPricing {
        input_per_mtok: f64::from(pricing.input_per_mtok_cents) / CENTS_PER_DOLLAR,
        output_per_mtok: f64::from(pricing.output_per_mtok_cents) / CENTS_PER_DOLLAR,
        cached_input_per_mtok: pricing
            .cached_input_per_mtok_cents
            .map(|cents| f64::from(cents) / CENTS_PER_DOLLAR),
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
    }
}

fn request_with_session_metadata(defer_brain_response: bool) -> CompletionRequest {
    let mut request = CompletionRequest::simple("hello").with_model("gpt-5.4");
    request.metadata.insert(
        "_moa.session_id".to_string(),
        json!(SessionId::new().to_string()),
    );
    if defer_brain_response {
        request
            .metadata
            .insert(DEFER_BRAIN_RESPONSE_METADATA_KEY.to_string(), json!(true));
    }
    request
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
