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
use moa_test_support::pricing::PricingTable;

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
            token_pricing_from_fixture("anthropic", "claude-sonnet-4-6"),
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
            token_pricing_from_fixture("openai", "gpt-4.1"),
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
            "gemini-3-flash-preview",
            token_pricing_from_fixture("gemini", "gemini-3-flash-preview"),
        ))),
    );

    let (provider_kind, model_id) = registry
        .resolve_provider_kind(Some("google:gemini-3-flash-preview"))
        .expect("prefixed google model should resolve");

    assert_eq!(provider_kind, ProviderKind::Google);
    assert_eq!(model_id.as_str(), "gemini-3-flash-preview");
}

#[test]
fn llm_gateway_resolve_provider_for_configured_static_default_model() {
    let registry = ProviderRegistry::with_static_providers(
        None,
        Some(Arc::new(MockProvider::success(
            "scripted",
            "scripted-loadtest",
            TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: Some(0.0),
            },
        ))),
        None,
    );

    let (provider_kind, model_id) = registry
        .resolve_provider_kind(Some("scripted-loadtest"))
        .expect("configured static default model should resolve");

    assert_eq!(provider_kind, ProviderKind::OpenAI);
    assert_eq!(model_id.as_str(), "scripted-loadtest");
}

#[test]
fn llm_gateway_compute_cost_cents_matches_pricing_table_v1_for_sonnet() {
    let model = "claude-sonnet-4-6";
    let usage = TokenUsage {
        input_tokens_uncached: 100_000,
        input_tokens_cache_write: 25_000,
        input_tokens_cache_read: 50_000,
        output_tokens: 20_000,
    };
    let table = PricingTable::load_v1();
    let expected = table
        .cost_cents(
            "anthropic",
            model,
            (usage.input_tokens_uncached + usage.input_tokens_cache_write) as u64,
            usage.output_tokens as u64,
            usage.input_tokens_cache_read as u64,
        )
        .expect("sonnet pricing fixture");

    assert_eq!(compute_cost_cents(model, usage), expected);
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

fn token_pricing_from_fixture(provider: &str, model: &str) -> TokenPricing {
    let table = PricingTable::load_v1();
    let pricing = table
        .get(provider, model)
        .unwrap_or_else(|error| panic!("missing fixture pricing for {provider}/{model}: {error}"));
    TokenPricing {
        input_per_mtok: f64::from(pricing.input_per_mtok_cents) / CENTS_PER_DOLLAR,
        output_per_mtok: f64::from(pricing.output_per_mtok_cents) / CENTS_PER_DOLLAR,
        cached_input_per_mtok: pricing
            .cached_input_per_mtok_cents
            .map(|cents| f64::from(cents) / CENTS_PER_DOLLAR),
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
