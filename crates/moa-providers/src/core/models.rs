//! Static catalog of LLM models MOA can route to, along with their pricing and
//! capability metadata. One source of truth consumed by:
//!
//!   * `moa-providers` factory — for validation when the user picks a
//!     model that needs a specific provider.
//!   * Hosted API and gateway/admin surfaces that need model capability metadata.
//!
//! Context windows and prices reflect public information as of 2026-04. Update
//! this file when providers ship new models, extend windows, or change pricing.

use std::time::Duration;

use moa_core::{
    MoaError, ModelCapabilities, ModelId, ProviderNativeTool, Result, TokenPricing, ToolCallFormat,
};

/// Identifier used in the catalog to denote the provider a model runs
/// under. Matches `factory`'s `PROVIDER_*` constants.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_GOOGLE: &str = "google";

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
}

impl ProviderModel {
    /// Builds `moa-core` model capabilities from this catalog entry.
    #[must_use]
    pub fn capabilities_with_native_tools(
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
/// Context-window numbers reflect 2026-04 provider docs:
/// Claude Opus/Sonnet 4.6 → 1M; Haiku 4.5 → 200K; GPT-5.4 → 1.05M;
/// GPT-5.4 mini → 400K; GPT-4o → 128K; Gemini 3 family → ~1.05M.
pub const CATALOG: &[ProviderModel] = &[
    // ---- Anthropic ----
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
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output_tokens: 64_000,
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
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output_tokens: 16_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: Some(300),
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 0.8,
            output_per_mtok: 4.0,
            cached_input_per_mtok: Some(0.08),
            cache_write_5m_per_mtok: Some(1.0),
            cache_write_1h_per_mtok: Some(1.6),
        },
    },
    // ---- OpenAI ----
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
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 12.0,
            cached_input_per_mtok: Some(0.2),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3-pro-preview",
        display_name: "Gemini 3 Pro",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 2.0,
            output_per_mtok: 12.0,
            cached_input_per_mtok: Some(0.2),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3-flash-preview",
        display_name: "Gemini 3 Flash",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.5,
            output_per_mtok: 3.0,
            cached_input_per_mtok: Some(0.05),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3.1-flash-lite-preview",
        display_name: "Gemini 3.1 Flash-Lite",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl_secs: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 1.5,
            cached_input_per_mtok: Some(0.025),
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
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
    fn google_catalog_includes_latest_gemini_3_series() {
        assert!(find("gemini-3.1-pro-preview").is_some());
        assert!(find("gemini-3-pro-preview").is_some());
        assert!(find("gemini-3-flash-preview").is_some());
        assert!(find("gemini-3.1-flash-lite-preview").is_some());
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
    fn embedding_and_rerank_model_ids_are_uncosted_via_catalog() {
        // Pins (intentional gap): the chat CATALOG/TokenPricing models token-billed
        // completion models only. Embedding and rerank ids are deliberately absent,
        // so `find_model`/`pricing_for_model` return `None` for them and their cost
        // is accounted elsewhere. This guards against a half-wired entry that would
        // expose chat token pricing for a non-chat model.
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
        // The 2026-04 catalog's cheapest combined price is GPT-5 nano.
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
