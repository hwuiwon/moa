//! Static catalog of LLM models MOA can route to, along with their
//! context-window sizes. One source of truth consumed by:
//!
//!   * `moa-providers` factory — for validation when the user picks a
//!     model that needs a specific provider.
//!   * `moa-desktop` settings — for the Model dropdown in General.
//!   * `moa-desktop` detail panel — for the context-usage progress bar.
//!
//! Context windows reflect public information as of 2026-04. Update
//! this file (and nothing else) when providers ship new models or
//! extend the windows of existing ones.

/// Identifier used in the catalog to denote the provider a model runs
/// under. Matches `factory`'s `PROVIDER_*` constants.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_OPENAI: &str = "openai";
pub const PROVIDER_GOOGLE: &str = "google";

/// One catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderModel {
    /// Provider that serves the model (`"anthropic"` / `"openai"` /
    /// `"google"`).
    pub provider: &'static str,
    /// Canonical model id passed to the provider API. **This is what
    /// gets written into `MoaConfig.general.default_model`.**
    pub id: &'static str,
    /// Human-readable label shown in dropdowns.
    pub display_name: &'static str,
    /// Maximum input-context window size in tokens. Used as the
    /// denominator of the context-usage progress bar.
    pub context_window: usize,
    /// Maximum output tokens per response. Surfaced for reference but
    /// not yet enforced client-side.
    pub max_output_tokens: usize,
}

/// Full catalog, ordered provider-then-capability so downstream
/// dropdowns don't need a separate sort step.
///
/// Context-window numbers reflect 2026-04 provider docs:
/// Claude Opus/Sonnet 4.6 → 1M; Haiku 4.5 → 200K; GPT-5.4 → 1.05M;
/// GPT-5.4 mini → 400K; GPT-4o → 128K; Gemini 2.5 family → ~1.05M.
pub const CATALOG: &[ProviderModel] = &[
    // ---- Anthropic ----
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        context_window: 1_000_000,
        max_output_tokens: 64_000,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        context_window: 200_000,
        max_output_tokens: 16_000,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5 (prev)",
        context_window: 200_000,
        max_output_tokens: 64_000,
    },
    ProviderModel {
        provider: PROVIDER_ANTHROPIC,
        id: "claude-sonnet-4",
        display_name: "Claude Sonnet 4 (prev)",
        context_window: 200_000,
        max_output_tokens: 64_000,
    },
    // ---- OpenAI ----
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.4",
        display_name: "GPT-5.4",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5.4-mini",
        display_name: "GPT-5.4 mini",
        context_window: 400_000,
        max_output_tokens: 64_000,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-5",
        display_name: "GPT-5 (prev)",
        context_window: 400_000,
        max_output_tokens: 128_000,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "gpt-4o",
        display_name: "GPT-4o (legacy)",
        context_window: 128_000,
        max_output_tokens: 16_384,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "o3",
        display_name: "o3 (reasoning)",
        context_window: 200_000,
        max_output_tokens: 100_000,
    },
    ProviderModel {
        provider: PROVIDER_OPENAI,
        id: "o4-mini",
        display_name: "o4-mini (reasoning)",
        context_window: 200_000,
        max_output_tokens: 100_000,
    },
    // ---- Google ----
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-2.5-flash",
        display_name: "Gemini 2.5 Flash",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-2.5-flash-lite",
        display_name: "Gemini 2.5 Flash-Lite",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3-flash",
        display_name: "Gemini 3 Flash",
        context_window: 1_048_576,
        max_output_tokens: 65_536,
    },
    ProviderModel {
        provider: PROVIDER_GOOGLE,
        id: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        context_window: 2_000_000,
        max_output_tokens: 65_536,
    },
];

/// Returns the catalog entry for a model id, if known.
pub fn find(model_id: &str) -> Option<&'static ProviderModel> {
    CATALOG.iter().find(|m| m.id == model_id)
}

/// Returns the context-window size for a model id, or `None` if the id
/// isn't in the catalog.
pub fn context_window(model_id: &str) -> Option<usize> {
    find(model_id).map(|m| m.context_window)
}

/// Returns every model served by a given provider name.
pub fn by_provider(provider: &str) -> impl Iterator<Item = &'static ProviderModel> {
    CATALOG.iter().filter(move |m| m.provider == provider)
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
}
