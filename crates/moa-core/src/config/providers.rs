//! Provider and model routing configuration.

use serde::{Deserialize, Serialize};

use crate::error::MoaError;

/// Where provider in-flight concurrency is coordinated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConcurrencyScope {
    /// Each process enforces its own in-flight ceiling (single-node/dev default).
    #[default]
    Local,
    /// The ceiling is shared across replicas via the runtime coordination store,
    /// so an autoscaled fleet does not multiply a shared provider/API-key quota.
    Global,
}

/// In-flight concurrency limits applied to outbound provider calls.
///
/// Provider rate limits are tied to the account's tier (Anthropic tier N, a
/// Cohere trial vs production key, etc.), so the in-flight ceiling is expressed
/// **per provider** via that provider's `max_concurrent_requests` — the natural
/// place an operator states their tier. That one budget is shared across every
/// call kind the credential serves (e.g. Cohere embed + rerank on one key).
/// [`default_max_in_flight`](Self::default_max_in_flight) is the workspace-wide
/// fallback for any provider that sets no explicit limit, so nothing is unbounded
/// by accident (`0` = explicitly unbounded). `scope` selects process-local
/// enforcement (default) or cross-replica coordination through the runtime store;
/// `global` scope additionally uses `lease_ttl_ms` as the crash backstop for a
/// held slot (see the limiter's TTL derivation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConcurrencyConfig {
    /// Whether the ceiling is enforced per process or shared across replicas.
    pub scope: ConcurrencyScope,
    /// Fallback in-flight ceiling for any provider that sets no
    /// `max_concurrent_requests` of its own (`0` = unbounded). Operators express
    /// their per-provider account tier via each provider's own setting; this
    /// keeps unconfigured providers bounded rather than unbounded.
    pub default_max_in_flight: u32,
    /// How long a caller waits for a slot before reporting "saturated", in ms.
    pub block_threshold_ms: u64,
    /// Global-scope lease time-to-live, in ms: the crash backstop for a held slot.
    ///
    /// Must exceed the longest provider call. Non-streaming calls are bounded by
    /// the 60s HTTP request timeout; streaming chat has no whole-request timeout,
    /// so this is the operator-tunable ceiling for the longest expected stream. A
    /// killed replica's slots self-expire after this TTL.
    pub lease_ttl_ms: u64,
}

impl Default for ProviderConcurrencyConfig {
    fn default() -> Self {
        Self {
            scope: ConcurrencyScope::Local,
            default_max_in_flight: 16,
            block_threshold_ms: 2_000,
            lease_ttl_ms: 600_000,
        }
    }
}

impl ProviderConcurrencyConfig {
    /// Validates the concurrency settings, rejecting nonsensical durations.
    pub fn validate(&self) -> Result<(), MoaError> {
        if self.block_threshold_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.concurrency.block_threshold_ms must be greater than zero".to_string(),
            ));
        }
        if self.scope == ConcurrencyScope::Global && self.lease_ttl_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.concurrency.lease_ttl_ms must be greater than zero for global scope"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

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
    /// In-flight concurrency ceiling for this provider account — the natural
    /// place to express the credential's tier. This one budget is shared across
    /// every call kind the credential serves (chat, embedding, rerank). `None`
    /// falls back to `providers.concurrency.default_max_in_flight`; an explicit
    /// `0` opts back into unbounded.
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
    /// In-flight concurrency limits and coordination scope for provider calls.
    #[serde(default)]
    pub concurrency: ProviderConcurrencyConfig,
}

impl ProvidersConfig {
    /// Validates provider configuration, currently the concurrency settings.
    pub fn validate(&self) -> Result<(), MoaError> {
        self.concurrency.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConcurrencyScope, ProviderConcurrencyConfig, ProviderCredentialConfig, ProvidersConfig,
    };

    #[test]
    fn provider_concurrency_config_defaults_to_local_bounded_scope() {
        // Pins: single-node/dev behavior defaults to process-local scope with a
        // bounded fallback ceiling (nothing unbounded by accident), and validates.
        let config = ProviderConcurrencyConfig::default();
        assert_eq!(config.scope, ConcurrencyScope::Local);
        assert_eq!(config.default_max_in_flight, 16);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn provider_concurrency_config_parses_global_scope_and_rejects_bad_durations() {
        // Pins: operators opt into global coordination via config; a zero block
        // threshold and a zero global lease TTL are rejected.
        let parsed: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "concurrency": { "scope": "global", "default_max_in_flight": 64, "lease_ttl_ms": 300000 }
        }))
        .expect("providers config with global concurrency should parse");
        assert_eq!(parsed.concurrency.scope, ConcurrencyScope::Global);
        assert_eq!(parsed.concurrency.default_max_in_flight, 64);
        assert!(parsed.validate().is_ok());

        let bad = ProviderConcurrencyConfig {
            block_threshold_ms: 0,
            ..ProviderConcurrencyConfig::default()
        };
        assert!(bad.validate().is_err(), "zero block threshold is invalid");

        let mut bad_ttl = ProviderConcurrencyConfig {
            scope: ConcurrencyScope::Global,
            lease_ttl_ms: 0,
            ..ProviderConcurrencyConfig::default()
        };
        assert!(
            bad_ttl.validate().is_err(),
            "global scope with zero lease TTL is invalid"
        );
        bad_ttl.scope = ConcurrencyScope::Local;
        assert!(
            bad_ttl.validate().is_ok(),
            "a zero lease TTL is only rejected under global scope"
        );
    }

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
