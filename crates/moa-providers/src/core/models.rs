//! Static catalog of LLM models MOA can route to, along with their pricing and
//! capability metadata. One source of truth consumed by:
//!
//!   * `moa-providers` factory — for validation when the user picks a
//!     model that needs a specific provider.
//!   * Hosted API and gateway/admin surfaces that need model capability metadata.
//!
//! Context windows and prices reflect public provider documentation verified on
//! 2026-07-02. Update this file when providers ship new models, extend windows,
//! or change pricing.

use std::time::Duration;

use moa_core::{
    error::MoaError, error::Result, types::identifiers::ModelId, types::model::ModelCapabilities,
    types::model::ProviderNativeTool, types::model::TokenPricing, types::model::ToolCallFormat,
};

/// Identifier used in the catalog to denote the provider a model runs
/// under. Matches `factory`'s `PROVIDER_*` constants.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_GOOGLE: &str = "google";

/// Capability class of a chat model, used to keep LLM failover within a nearby
/// capability band (a request should not fail over from, say, a frontier model
/// to a light one). Ranked 0 (most capable) through 4 (least).
///
/// This is intentionally distinct from [`moa_core::types::provider::ModelTier`], which is a coarse
/// pricing/analytics tier (`Main`/`Auxiliary`); this is a finer capability ladder
/// used for routing/failover decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    /// Most capable frontier models.
    Frontier,
    /// Top general-purpose models just below frontier.
    Flagship,
    /// Balanced cost/capability workhorses.
    Balanced,
    /// Fast, lower-cost models.
    Fast,
    /// Lightest, cheapest models.
    Light,
}

impl CapabilityTier {
    /// Returns the capability rank (0 = most capable, 4 = least).
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Frontier => 0,
            Self::Flagship => 1,
            Self::Balanced => 2,
            Self::Fast => 3,
            Self::Light => 4,
        }
    }

    /// Returns the number of capability tiers between two models.
    #[must_use]
    pub fn distance(self, other: Self) -> u8 {
        self.rank().abs_diff(other.rank())
    }

    /// Returns a stable lowercase label for diagnostics and error messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frontier => "frontier",
            Self::Flagship => "flagship",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
            Self::Light => "light",
        }
    }
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderModel {
    /// Provider that serves the model (`"anthropic"` / `"openai"` /
    /// `"google"`).
    pub provider: &'static str,
    /// Canonical model id passed to the provider API. **This is what
    /// gets written into `MoaConfig.models.main`.**
    pub id: &'static str,
    /// Human-readable label shown in dropdowns.
    pub display_name: &'static str,
    /// Maximum input-context window size in tokens. Used as the
    /// denominator of the context-usage progress bar.
    pub context_window: usize,
    /// Maximum output tokens per response. Surfaced for reference but
    /// not yet enforced client-side.
    pub max_output_tokens: usize,
    /// Whether this model can call tools.
    pub supports_tools: bool,
    /// Whether this model accepts vision input.
    pub supports_vision: bool,
    /// Whether this model supports provider-side prompt prefix caching.
    pub supports_prefix_caching: bool,
    /// Prompt cache time-to-live in seconds when known.
    pub cache_ttl_secs: Option<u64>,
    /// Provider-specific tool-call encoding.
    pub tool_call_format: ToolCallFormat,
    /// Token pricing for cost analytics.
    pub pricing: TokenPricing,
    /// Capability tier used to constrain failover to a nearby capability band.
    pub tier: CapabilityTier,
}

impl ProviderModel {
    /// Builds `moa-core` model capabilities from this catalog entry.
    #[must_use]
    fn capabilities_with_native_tools(
        &self,
        native_tools: Vec<ProviderNativeTool>,
    ) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new(self.id),
            context_window: self.context_window,
            max_output: self.max_output_tokens,
            supports_tools: self.supports_tools,
            supports_vision: self.supports_vision,
            supports_prefix_caching: self.supports_prefix_caching,
            cache_ttl: self.cache_ttl_secs.map(Duration::from_secs),
            tool_call_format: self.tool_call_format.clone(),
            pricing: self.pricing.clone(),
            native_tools,
        }
    }
}

/// Full catalog, ordered provider-then-capability so downstream
/// dropdowns don't need a separate sort step.
///
/// Context windows/prices verified against provider docs on 2026-07-02.
/// Anthropic cache pricing convention: cached read = 0.1x input, 5m cache write =
/// 1.25x input, 1h cache write = 2x input.
pub const CATALOG: &[ProviderModel] = &[
    // ---- Anthropic ----
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-fable-5",
        display_name: "Claude Fable 5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 10.0,
            output_per_mtok: 50.0,
            cached_input_per_mtok: Some(1.0),
            cache_write_5m_per_mtok: Some(12.5),
            cache_write_1h_per_mtok: Some(20.0),
        },
        tier: CapabilityTier::Frontier,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cached_input_per_mtok: Some(0.5),
            cache_write_5m_per_mtok: Some(6.25),
            cache_write_1h_per_mtok: Some(10.0),
        },
        tier: CapabilityTier::Flagship,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cached_input_per_mtok: Some(0.5),
            cache_write_5m_per_mtok: Some(6.25),
            cache_write_1h_per_mtok: Some(10.0),
        },
        tier: CapabilityTier::Flagship,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cached_input_per_mtok: Some(0.5),
            cache_write_5m_per_mtok: Some(6.25),
            cache_write_1h_per_mtok: Some(10.0),
        },
        tier: CapabilityTier::Flagship,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        // The catalog carries standard pricing; introductory pricing of
        // $2/$10 per MTok runs through 2026-08-31.
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: Some(6.0),
        },
        tier: CapabilityTier::Balanced,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: Some(6.0),
        },
        tier: CapabilityTier::Balanced,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output_tokens: 64_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cached_input_per_mtok: Some(0.10),
            cache_write_5m_per_mtok: Some(1.25),
            cache_write_1h_per_mtok: Some(2.0),
        },
        tier: CapabilityTier::Fast,
    },
    // ---- OpenAI ----
    // Excluded until GA: gpt-5.5-pro ($30/$180) and the GPT-5.6 Sol/Terra/Luna
    // family (limited preview).
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.5",
        display_name: "GPT-5.5",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 5.0,
            output_per_mtok: 30.0,
            cached_input_per_mtok: Some(0.50),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Frontier,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.4",
        display_name: "GPT-5.4",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 2.50,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.25),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Flagship,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.4-mini",
        display_name: "GPT-5.4 mini",
        context_window: 400_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.75,
            output_per_mtok: 4.50,
            cached_input_per_mtok: Some(0.075),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Fast,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.4-nano",
        display_name: "GPT-5.4 nano",
        context_window: 400_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.20,
            output_per_mtok: 1.25,
            cached_input_per_mtok: Some(0.02),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Light,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5-mini",
        display_name: "GPT-5 mini",
        context_window: 400_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 2.0,
            cached_input_per_mtok: Some(0.025),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Fast,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5-nano",
        display_name: "GPT-5 nano",
        context_window: 400_000,
        max_output_tokens: 128_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.05,
            output_per_mtok: 0.40,
            cached_input_per_mtok: Some(0.005),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Light,
    },
    // ---- Google ----
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3.1-pro-preview",
        display_name: "Gemini 3.1 Pro",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 12.0,
            cached_input_per_mtok: Some(0.2),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Frontier,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3-pro-preview",
        display_name: "Gemini 3 Pro",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 12.0,
            cached_input_per_mtok: Some(0.2),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Flagship,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3.5-flash",
        display_name: "Gemini 3.5 Flash",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 1.5,
            output_per_mtok: 9.0,
            cached_input_per_mtok: Some(0.15),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Balanced,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3-flash-preview",
        display_name: "Gemini 3 Flash",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.5,
            output_per_mtok: 3.0,
            cached_input_per_mtok: Some(0.05),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Fast,
    },
    // Flash-Lite is now GA (no `-preview` suffix on the pricing page).
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3.1-flash-lite",
        display_name: "Gemini 3.1 Flash-Lite",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: false,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 1.5,
            cached_input_per_mtok: Some(0.025),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        tier: CapabilityTier::Light,
    },
];

/// Returns the catalog entry for a model id, if known.
pub fn find(model_id: &str) -> Option<&'static ProviderModel> {
    CATALOG.iter().find(|m| m.id == model_id)
}

/// Returns the catalog entry for a provider/model pair, using longest-prefix matching.
pub fn find_for_provider_model(provider: &str, model_id: &str) -> Option<&'static ProviderModel> {
    CATALOG
        .iter()
        .filter(|model| model.provider == provider)
        .filter(|model| model_id == model.id || model_id.starts_with(model.id))
        .max_by_key(|model| model.id.len())
}

/// Returns the catalog entry for any provider model id, using longest-prefix matching.
pub fn find_model(model_id: &str) -> Option<&'static ProviderModel> {
    CATALOG
        .iter()
        .filter(|model| model_id == model.id || model_id.starts_with(model.id))
        .max_by_key(|model| model.id.len())
}

/// Returns the context-window size for a model id, or `None` if the id
/// isn't in the catalog.
pub fn context_window(model_id: &str) -> Option<usize> {
    find_model(model_id).map(|m| m.context_window)
}

/// Returns every model served by a given provider name.
pub fn by_provider(provider: &str) -> impl Iterator<Item = &'static ProviderModel> {
    CATALOG.iter().filter(move |m| m.provider == provider)
}

/// Returns model capabilities for a provider/model pair.
pub fn capabilities_for_provider_model(
    provider: &str,
    model_id: &str,
    native_tools: Vec<ProviderNativeTool>,
) -> Result<ModelCapabilities> {
    find_for_provider_model(provider, model_id)
        .map(|model| model.capabilities_with_native_tools(native_tools))
        .ok_or_else(|| MoaError::Unsupported(format!("unsupported {provider} model '{model_id}'")))
}

/// Returns `model` unchanged when it is catalogued for `provider`, otherwise an
/// [`MoaError::Unsupported`] error that names the provider via `display_name`.
pub fn canonical_model_id(provider: &str, display_name: &str, model: &str) -> Result<String> {
    if find_for_provider_model(provider, model).is_some() {
        return Ok(model.to_string());
    }

    Err(MoaError::Unsupported(format!(
        "unsupported {display_name} model '{model}'"
    )))
}

/// Returns token pricing for a model id from the catalog.
pub fn pricing_for_model(model_id: &str) -> Option<TokenPricing> {
    find_model(model_id).map(|model| model.pricing.clone())
}

/// Combined per-MTok input plus output price used to rank chat models by cost.
fn chat_price_rank(model: &ProviderModel) -> f64 {
    model.pricing.input_per_mtok + model.pricing.output_per_mtok
}

/// Returns the cheapest chat-capable catalog model by combined input+output
/// token price.
///
/// Every [`CATALOG`] entry is a token-billed chat completion model (embedding
/// and rerank ids are deliberately absent), so this ranks the whole catalog by
/// `input_per_mtok + output_per_mtok` and returns the minimum. Returns `None`
/// only if the catalog is empty.
pub fn cheapest_chat_model() -> Option<&'static ProviderModel> {
    CATALOG
        .iter()
        .min_by(|left, right| chat_price_rank(left).total_cmp(&chat_price_rank(right)))
}

/// One embedding-model pricing entry.
///
/// Embedding and rerank calls are billed per input token or per search unit,
/// not per chat completion (context window, output-token limit, tool
/// support), so their pricing lives in this dedicated table rather than as
/// [`CATALOG`] entries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmbeddingModelPrice {
    /// Provider that serves the model (`"openai"` / `"cohere"` / `"google"` /
    /// `"zeroentropy"`).
    pub provider: &'static str,
    /// Canonical embedding model id passed to the provider API.
    pub id: &'static str,
    /// Price in USD per 1,000,000 input tokens.
    pub price_per_mtok: f64,
}

/// One rerank-model pricing entry, billed per "search" (one query over up to
/// 100 documents, per Cohere's billing definition) rather than per token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankModelPrice {
    /// Provider that serves the model (`"cohere"` / `"zeroentropy"`).
    pub provider: &'static str,
    /// Canonical rerank model id passed to the provider API.
    pub id: &'static str,
    /// Price in USD per 1,000 search units.
    pub price_per_thousand_searches: f64,
}

/// Embedding-model pricing, verified against provider pricing pages on
/// 2026-07-12: OpenAI (platform.openai.com/docs/api-reference/embeddings),
/// Cohere (cohere.com/pricing, embeddingcost.com/cohere), Google
/// (ai.google.dev/gemini-api/docs/pricing), ZeroEntropy
/// (zeroentropy.dev/pricing).
pub const EMBEDDING_CATALOG: &[EmbeddingModelPrice] = &[
    EmbeddingModelPrice {
        provider: PROVIDER_OPENAI,
        id: "text-embedding-3-small",
        price_per_mtok: 0.02,
    },
    EmbeddingModelPrice {
        provider: PROVIDER_OPENAI,
        id: "text-embedding-3-large",
        price_per_mtok: 0.13,
    },
    EmbeddingModelPrice {
        provider: PROVIDER_OPENAI,
        id: "text-embedding-ada-002",
        price_per_mtok: 0.10,
    },
    EmbeddingModelPrice {
        provider: "cohere",
        id: "embed-v4.0",
        price_per_mtok: 0.12,
    },
    EmbeddingModelPrice {
        provider: "cohere",
        id: "embed-english-v3.0",
        price_per_mtok: 0.10,
    },
    EmbeddingModelPrice {
        provider: PROVIDER_GOOGLE,
        id: "gemini-embedding-2",
        price_per_mtok: 0.20,
    },
    EmbeddingModelPrice {
        provider: "zeroentropy",
        id: "zembed-1",
        price_per_mtok: 0.05,
    },
];

/// Rerank-model pricing, verified against provider pricing pages on
/// 2026-07-12.
pub const RERANK_CATALOG: &[RerankModelPrice] = &[
    RerankModelPrice {
        provider: "cohere",
        id: "rerank-v3.5",
        price_per_thousand_searches: 2.00,
    },
    RerankModelPrice {
        provider: "cohere",
        id: "rerank-v4.0",
        price_per_thousand_searches: 2.00,
    },
    RerankModelPrice {
        provider: "cohere",
        id: "rerank-v4.0-fast",
        price_per_thousand_searches: 2.00,
    },
    RerankModelPrice {
        provider: "zeroentropy",
        // TODO(pricing): unverified in this unit. ZeroEntropy bills zerank-2 at
        // $0.025 per 1,000,000 tokens (confirmed at zeroentropy.dev/pricing),
        // not per search, and publishes no official token-to-search
        // conversion, so it cannot be priced in this per-1K-searches table
        // without fabricating a ratio. Cost wiring skips this model (price is
        // 0.0) until ZeroEntropy either bills per search or MOA adds a
        // token-billed rerank pricing path.
        id: "zerank-2",
        price_per_thousand_searches: 0.0,
    },
];

/// Returns the USD price per 1,000,000 input tokens for an embedding model
/// id, or `None` if the id isn't catalogued.
pub fn embedding_price_per_mtok(model_id: &str) -> Option<f64> {
    EMBEDDING_CATALOG
        .iter()
        .find(|entry| entry.id == model_id)
        .map(|entry| entry.price_per_mtok)
}

/// Returns the USD price per 1,000 rerank search units for a rerank model id,
/// or `None` if the id isn't catalogued. A catalogued-but-unverified model
/// (see the `TODO(pricing)` entries in [`RERANK_CATALOG`]) returns
/// `Some(0.0)`; callers that skip zero-cost billing already treat that
/// correctly as "do not charge."
pub fn rerank_price_per_thousand_searches(model_id: &str) -> Option<f64> {
    RERANK_CATALOG
        .iter()
        .find(|entry| entry.id == model_id)
        .map(|entry| entry.price_per_thousand_searches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_unique_ids() {
        let mut ids: Vec<&'static str> = CATALOG.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let len_before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), len_before, "duplicate model id in catalog");
    }

    #[test]
    fn claude_opus_has_million_token_context() {
        let model = find("claude-opus-4-6").expect("Opus 4.6 must be catalogued");
        assert_eq!(model.context_window, 1_000_000);
    }

    #[test]
    fn gpt_5_4_has_extended_context() {
        let model = find("gpt-5.4").expect("GPT-5.4 must be catalogued");
        assert!(
            model.context_window >= 1_000_000,
            "GPT-5.4 should expose the 1M extended window"
        );
    }

    #[test]
    fn by_provider_partitions_correctly() {
        let anthropic_count = by_provider(PROVIDER_ANTHROPIC).count();
        let openai_count = by_provider(PROVIDER_OPENAI).count();
        let google_count = by_provider(PROVIDER_GOOGLE).count();
        assert_eq!(
            anthropic_count + openai_count + google_count,
            CATALOG.len(),
            "catalog has an entry with unknown provider"
        );
    }

    #[test]
    fn google_catalog_includes_latest_gemini_series() {
        assert!(find("gemini-3.1-pro-preview").is_some());
        assert!(find("gemini-3-pro-preview").is_some());
        assert!(find("gemini-3.5-flash").is_some());
        assert!(find("gemini-3-flash-preview").is_some());
        assert!(find("gemini-3.1-flash-lite").is_some());
    }

    #[test]
    fn anthropic_catalog_reflects_the_2026_07_refresh() {
        // Pins: the refreshed Anthropic pricing/limits and new models.
        assert!(find("claude-fable-5").is_some());
        assert!(find("claude-opus-4-8").is_some());
        assert!(find("claude-opus-4-7").is_some());
        assert!(find("claude-sonnet-5").is_some());

        let haiku = find("claude-haiku-4-5").expect("Haiku 4.5 catalogued");
        assert_eq!(haiku.pricing.input_per_mtok, 1.0);
        assert_eq!(haiku.pricing.output_per_mtok, 5.0);
        assert_eq!(haiku.pricing.cached_input_per_mtok, Some(0.10));
        assert_eq!(haiku.max_output_tokens, 64_000);

        let sonnet = find("claude-sonnet-4-6").expect("Sonnet 4.6 catalogued");
        assert_eq!(sonnet.max_output_tokens, 128_000);
    }

    #[test]
    fn capability_tier_distance_is_symmetric_and_ranked() {
        // Pins: tier distance is the absolute rank difference, so adjacent tiers
        // are distance 1 and frontier↔fast is distance 3.
        assert_eq!(
            CapabilityTier::Frontier.distance(CapabilityTier::Flagship),
            1
        );
        assert_eq!(CapabilityTier::Flagship.distance(CapabilityTier::Fast), 2);
        assert_eq!(CapabilityTier::Frontier.distance(CapabilityTier::Fast), 3);
        assert_eq!(
            CapabilityTier::Balanced.distance(CapabilityTier::Balanced),
            0
        );
    }

    #[test]
    fn every_catalog_model_is_accepted_by_its_adapter() {
        // Pins: the catalog cannot list unroutable provider models.
        for model in CATALOG {
            let resolved = match model.provider {
                PROVIDER_ANTHROPIC => {
                    crate::adapters::anthropic::model::canonical_model_id(model.id)
                }
                PROVIDER_OPENAI => {
                    crate::adapters::openai_responses::provider::canonical_model_id(model.id)
                }
                PROVIDER_GOOGLE => crate::adapters::gemini::model::canonical_model_id(model.id),
                unsupported => panic!("unknown provider in catalog: {unsupported}"),
            }
            .unwrap_or_else(|error| {
                panic!(
                    "catalog model {}/{} was rejected by its adapter: {error}",
                    model.provider, model.id
                )
            });

            assert_eq!(resolved, model.id);
        }
    }

    #[test]
    fn find_model_resolves_dated_snapshot_to_base_via_longest_prefix() {
        // Pins: a dated provider snapshot id resolves to its base catalog entry,
        // and the longest matching prefix wins over a shorter sibling prefix.
        let dated = find_model("claude-sonnet-4-6-20260101")
            .expect("dated Sonnet snapshot should resolve to the base model");
        assert_eq!(dated.id, "claude-sonnet-4-6");

        // `gpt-5-mini-2026-01-01` shares the `gpt-5-` stem with `gpt-5-nano`, but
        // only `gpt-5-mini` is an actual prefix, so the more specific id wins.
        let mini = find_model("gpt-5-mini-2026-01-01").expect("dated GPT-5 mini should resolve");
        assert_eq!(mini.id, "gpt-5-mini");

        // An id with no catalog prefix resolves to nothing rather than a partial match.
        assert!(find_model("claude-imaginary-9").is_none());
    }

    #[test]
    fn embedding_and_rerank_model_ids_are_priced_via_dedicated_catalogs() {
        // Pins: the chat CATALOG/TokenPricing models token-billed completion
        // models only, so embedding and rerank ids stay absent from it and
        // `find_model`/`pricing_for_model` return `None` for them — this guards
        // against a half-wired entry that would expose chat token pricing for a
        // non-chat model. Embedding/rerank cost is instead wired through the
        // dedicated EMBEDDING_CATALOG/RERANK_CATALOG tables, which this test
        // asserts price the real ids.
        for id in [
            "embed-v4.0",
            "zembed-1",
            "gemini-embedding-2",
            "text-embedding-3-small",
            "zerank-2",
            "rerank-v4.0-fast",
        ] {
            assert!(
                find_model(id).is_none(),
                "{id} should not be in the chat catalog"
            );
            assert!(
                pricing_for_model(id).is_none(),
                "{id} should be uncosted via the chat catalog"
            );
        }

        assert_eq!(
            embedding_price_per_mtok("text-embedding-3-small"),
            Some(0.02)
        );
        assert_eq!(
            embedding_price_per_mtok("text-embedding-3-large"),
            Some(0.13)
        );
        assert_eq!(embedding_price_per_mtok("embed-v4.0"), Some(0.12));
        assert_eq!(embedding_price_per_mtok("gemini-embedding-2"), Some(0.20));
        assert_eq!(embedding_price_per_mtok("zembed-1"), Some(0.05));
        assert_eq!(embedding_price_per_mtok("not-a-real-embedding-model"), None);

        assert_eq!(
            rerank_price_per_thousand_searches("rerank-v4.0-fast"),
            Some(2.00)
        );
        assert_eq!(
            rerank_price_per_thousand_searches("rerank-v3.5"),
            Some(2.00)
        );
        assert_eq!(
            rerank_price_per_thousand_searches("not-a-real-rerank-model"),
            None
        );
    }

    #[test]
    fn cheapest_chat_model_picks_min_combined_price() {
        // Pins: narration's default model is the catalog's lowest input+output priced chat model.
        let cheapest = cheapest_chat_model().expect("catalog is non-empty");
        let cheapest_rank = chat_price_rank(cheapest);
        for model in CATALOG {
            assert!(
                cheapest_rank <= chat_price_rank(model),
                "{} (rank {}) is cheaper than the selected {} (rank {cheapest_rank})",
                model.id,
                chat_price_rank(model),
                cheapest.id,
            );
        }
        // The refreshed catalog's cheapest combined price is still GPT-5 nano
        // ($0.05 + $0.40 = $0.45/MTok).
        assert_eq!(cheapest.id, "gpt-5-nano");
    }

    #[test]
    fn pricing_lookup_uses_catalog_entry() {
        // Pins: model pricing is read from catalog metadata, including cache-write rates.
        let pricing = pricing_for_model("claude-sonnet-4-6").expect("sonnet pricing");

        assert_eq!(pricing.input_per_mtok, 3.0);
        assert_eq!(pricing.output_per_mtok, 15.0);
        assert_eq!(pricing.cached_input_per_mtok, Some(0.3));
        assert_eq!(pricing.cache_write_5m_per_mtok, Some(3.75));
        assert_eq!(pricing.cache_write_1h_per_mtok, Some(6.0));
    }
}
