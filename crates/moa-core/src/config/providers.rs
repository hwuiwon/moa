//! Provider and model routing configuration.

use serde::{Deserialize, Serialize};

/// General runtime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Default provider key.
    pub default_provider: String,
    /// Requested reasoning effort.
    pub reasoning_effort: String,
    /// Whether provider-native web search should be offered to supported models.
    pub web_search_enabled: bool,
    /// Optional repository-workspace instructions injected into the prompt.
    pub workspace_instructions: Option<String>,
    /// Optional user-level preferences injected into the prompt.
    pub user_instructions: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            reasoning_effort: "medium".to_string(),
            web_search_enabled: true,
            workspace_instructions: None,
            user_instructions: None,
        }
    }
}

/// Tiered model-routing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Default model for the primary user-facing agent loop.
    pub main: String,
    /// Optional lower-cost model for auxiliary tasks.
    pub auxiliary: Option<String>,
    /// Ordered fallback chain for the main-loop model, each `provider:model` or a
    /// bare model id. When a main-loop call is blocked by rate limiting before any
    /// tokens stream, the runtime fails over to the next entry in order. Empty
    /// (the default) disables failover.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            main: "gpt-5.4".to_string(),
            auxiliary: None,
            fallback_models: Vec::new(),
        }
    }
}

/// Provider credential configuration.
///
/// The optional per-minute rate caps override a provider's built-in in-process
/// pacer defaults (e.g. lowering Cohere rerank to a trial key's ceiling), and
/// `max_concurrent_requests` caps the number of outbound calls this provider
/// keeps in flight at once. All three are enforced per API key in-process, so a
/// multi-instance fleet sharing one key should divide the documented budget
/// across instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCredentialConfig {
    /// API key value loaded from runtime configuration.
    pub api_key: String,
    /// Optional per-minute request-rate cap; `None` keeps the provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_requests_per_min: Option<u32>,
    /// Optional per-minute input-rate cap; `None` keeps the provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inputs_per_min: Option<u32>,
    /// Optional in-flight concurrency cap; `None` keeps the provider default
    /// (embedding/rerank default to a small window; chat/LLM default unbounded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_requests: Option<u32>,
}

impl ProviderCredentialConfig {
    /// Creates a provider credential config with a direct API key value.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            max_requests_per_min: None,
            max_inputs_per_min: None,
            max_concurrent_requests: None,
        }
    }
}

impl Default for ProviderCredentialConfig {
    fn default() -> Self {
        Self::new("")
    }
}

/// Provider-specific configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Anthropic credentials.
    pub anthropic: ProviderCredentialConfig,
    /// `OpenAI` credentials.
    pub openai: ProviderCredentialConfig,
    /// Google Gemini credentials.
    pub google: ProviderCredentialConfig,
    /// Cohere credentials.
    pub cohere: ProviderCredentialConfig,
    /// ZeroEntropy credentials.
    pub zeroentropy: ProviderCredentialConfig,
}

impl super::MoaEnvOverlay {
    /// Applies provider, model, and general runtime environment overrides.
    pub(in crate::config) fn apply_provider_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{
            set_copy_if_some, set_if_some, set_option_if_some, set_vec_if_some,
        };

        set_if_some(
            &mut config.general.default_provider,
            &self.general_default_provider,
        );
        set_if_some(
            &mut config.general.reasoning_effort,
            &self.general_reasoning_effort,
        );
        set_copy_if_some(
            &mut config.general.web_search_enabled,
            self.general_web_search_enabled,
        );
        set_option_if_some(
            &mut config.general.workspace_instructions,
            &self.general_workspace_instructions,
        );
        set_option_if_some(
            &mut config.general.user_instructions,
            &self.general_user_instructions,
        );
        set_if_some(&mut config.models.main, &self.models_main);
        set_option_if_some(&mut config.models.auxiliary, &self.models_auxiliary);
        set_vec_if_some(
            &mut config.models.fallback_models,
            &self.models_fallback_models,
        );
        set_if_some(
            &mut config.providers.anthropic.api_key,
            &self.anthropic_api_key,
        );
        set_if_some(&mut config.providers.openai.api_key, &self.openai_api_key);
        set_if_some(&mut config.providers.google.api_key, &self.google_api_key);
        set_if_some(&mut config.providers.cohere.api_key, &self.cohere_api_key);
        set_if_some(
            &mut config.providers.zeroentropy.api_key,
            &self.zeroentropy_api_key,
        );

        self.apply_provider_rate_limit_overlay(config);

        set_if_some(&mut config.memory.extraction.api_key, &self.cohere_api_key);
        set_if_some(
            &mut config.memory.vector.embedder.cohere.api_key,
            &self.cohere_api_key,
        );
        set_if_some(
            &mut config.memory.vector.embedder.gemini.api_key,
            &self.google_api_key,
        );
        set_if_some(
            &mut config.memory.vector.embedder.zeroentropy.api_key,
            &self.zeroentropy_api_key,
        );
    }

    /// Applies per-provider rate and concurrency cap environment overrides.
    ///
    /// Each cap follows the flat `MOA_<PROVIDER>_MAX_*` env convention and only
    /// sets a value when the corresponding variable is present, leaving any
    /// file-configured cap in place otherwise.
    fn apply_provider_rate_limit_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_copy_option_if_some;

        let providers = &mut config.providers;
        set_copy_option_if_some(
            &mut providers.anthropic.max_requests_per_min,
            self.anthropic_max_requests_per_min,
        );
        set_copy_option_if_some(
            &mut providers.anthropic.max_inputs_per_min,
            self.anthropic_max_inputs_per_min,
        );
        set_copy_option_if_some(
            &mut providers.anthropic.max_concurrent_requests,
            self.anthropic_max_concurrent_requests,
        );
        set_copy_option_if_some(
            &mut providers.openai.max_requests_per_min,
            self.openai_max_requests_per_min,
        );
        set_copy_option_if_some(
            &mut providers.openai.max_inputs_per_min,
            self.openai_max_inputs_per_min,
        );
        set_copy_option_if_some(
            &mut providers.openai.max_concurrent_requests,
            self.openai_max_concurrent_requests,
        );
        set_copy_option_if_some(
            &mut providers.google.max_requests_per_min,
            self.google_max_requests_per_min,
        );
        set_copy_option_if_some(
            &mut providers.google.max_inputs_per_min,
            self.google_max_inputs_per_min,
        );
        set_copy_option_if_some(
            &mut providers.google.max_concurrent_requests,
            self.google_max_concurrent_requests,
        );
        set_copy_option_if_some(
            &mut providers.cohere.max_requests_per_min,
            self.cohere_max_requests_per_min,
        );
        set_copy_option_if_some(
            &mut providers.cohere.max_inputs_per_min,
            self.cohere_max_inputs_per_min,
        );
        set_copy_option_if_some(
            &mut providers.cohere.max_concurrent_requests,
            self.cohere_max_concurrent_requests,
        );
        set_copy_option_if_some(
            &mut providers.zeroentropy.max_requests_per_min,
            self.zeroentropy_max_requests_per_min,
        );
        set_copy_option_if_some(
            &mut providers.zeroentropy.max_inputs_per_min,
            self.zeroentropy_max_inputs_per_min,
        );
        set_copy_option_if_some(
            &mut providers.zeroentropy.max_concurrent_requests,
            self.zeroentropy_max_concurrent_requests,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderCredentialConfig;

    #[test]
    fn provider_credential_config_round_trips_rate_and_concurrency_caps() {
        // Pins: config carries the optional rate/concurrency caps so operators can
        // apply e.g. a trial key's ceilings without code, unset caps deserialize
        // to None (provider built-in defaults), and None caps are omitted on
        // serialize rather than written as explicit nulls.
        let parsed: ProviderCredentialConfig = serde_json::from_value(serde_json::json!({
            "api_key": "trial-key",
            "max_requests_per_min": 10,
            "max_inputs_per_min": 500,
            "max_concurrent_requests": 2,
        }))
        .expect("provider credential config should parse");

        assert_eq!(parsed.api_key, "trial-key");
        assert_eq!(parsed.max_requests_per_min, Some(10));
        assert_eq!(parsed.max_inputs_per_min, Some(500));
        assert_eq!(parsed.max_concurrent_requests, Some(2));

        let defaulted: ProviderCredentialConfig =
            serde_json::from_value(serde_json::json!({ "api_key": "only-key" }))
                .expect("bare api key should parse");
        assert_eq!(defaulted.max_requests_per_min, None);
        assert_eq!(defaulted.max_inputs_per_min, None);
        assert_eq!(defaulted.max_concurrent_requests, None);

        let serialized =
            serde_json::to_value(&defaulted).expect("defaulted config should serialize");
        assert_eq!(
            serialized,
            serde_json::json!({ "api_key": "only-key" }),
            "unset caps must be omitted, not serialized as null"
        );
    }
}
