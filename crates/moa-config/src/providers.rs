//! Provider and model routing configuration.

use serde::{Deserialize, Serialize};

use moa_core::error::MoaError;
use moa_core::types::provider::ProviderId;

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

/// What a distributed provider control does when coordination is unavailable.
///
/// "Unavailable" covers both a runtime-store failure at call time and a
/// deployment that declares a distributed scope without injecting a coordination
/// store at startup. It does **not** cover a deliberate
/// [`ConcurrencyScope::Local`] deployment, which is a configured choice rather
/// than a failure and stays silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinationFailurePolicy {
    /// Fall back to the process-local bound, with an explicit metric, a
    /// fleet-ceiling warning, and the degradation duration. Availability is
    /// preserved, but the effective ceiling is multiplied by the replica count
    /// while degraded, so this is only safe when the provider quota has headroom.
    #[default]
    BoundedDegraded,
    /// Reject admission instead of enforcing a ceiling that is no longer
    /// fleet-wide. Provider calls fail with a typed rate-limit error rather than
    /// risking a quota breach.
    FailClosed,
}

impl CoordinationFailurePolicy {
    /// Returns whether this policy rejects admission when coordination fails.
    #[must_use]
    pub const fn rejects_admission(self) -> bool {
        matches!(self, Self::FailClosed)
    }
}

/// Fleet coordination for per-minute pacing, 429 cooldown, and retry budget.
///
/// Concurrency admission has its own scope
/// ([`ProviderConcurrencyConfig::scope`]) because it is enforced with leases;
/// the controls here are enforced with shared counters and deadlines. Under
/// [`ConcurrencyScope::Global`] all three become fleet-wide through the runtime
/// coordination store, keyed by provider, opaque credential identity, model, and
/// rate class, so one API key's documented per-minute budget is not multiplied by
/// the replica count. The defaults reproduce the historical process-local
/// behavior exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderPacingConfig {
    /// Whether pacing, cooldown, and retry budget are per process or fleet-wide.
    pub scope: ConcurrencyScope,
    /// How long an idle shared pacing/retry key survives in the store, in ms.
    pub state_ttl_ms: u64,
    /// Upper bound on one pacing wait before the caller re-checks, in ms. Keeps
    /// a hostile or miscomputed refill estimate from parking a call indefinitely.
    pub max_pacing_wait_ms: u64,
    /// Cooldown applied to a rate-limit response with no `Retry-After`, in ms.
    pub default_cooldown_ms: u64,
    /// Ceiling on any single cooldown, in ms. A provider-supplied `Retry-After`
    /// longer than this is capped, so one hostile header cannot pause a whole
    /// fleet's access to a credential for an unbounded time.
    pub max_cooldown_ms: u64,
    /// Sliding window over which request and retry volume is measured, in ms.
    pub retry_budget_window_ms: u64,
    /// Percentage of window request volume that in-call retries may consume.
    pub retry_budget_percent: u32,
    /// Retries always allowed per window regardless of volume, so low-volume
    /// callers keep normal retry behavior.
    pub retry_budget_floor: u64,
}

impl Default for ProviderPacingConfig {
    fn default() -> Self {
        Self {
            scope: ConcurrencyScope::Local,
            state_ttl_ms: 300_000,
            max_pacing_wait_ms: 60_000,
            default_cooldown_ms: 5_000,
            max_cooldown_ms: 300_000,
            retry_budget_window_ms: 60_000,
            retry_budget_percent: 20,
            retry_budget_floor: 8,
        }
    }
}

impl ProviderPacingConfig {
    /// Returns whether pacing state is coordinated across replicas.
    #[must_use]
    pub const fn is_global(&self) -> bool {
        matches!(self.scope, ConcurrencyScope::Global)
    }

    /// Validates the pacing settings, rejecting budgets that cannot bound anything.
    pub fn validate(&self) -> Result<(), MoaError> {
        if self.retry_budget_percent == 0 || self.retry_budget_percent > 100 {
            return Err(MoaError::ConfigError(
                "providers.pacing.retry_budget_percent must be between 1 and 100".to_string(),
            ));
        }
        if self.retry_budget_window_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.pacing.retry_budget_window_ms must be greater than zero".to_string(),
            ));
        }
        if self.default_cooldown_ms == 0 || self.max_cooldown_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.pacing cooldown durations must be greater than zero".to_string(),
            ));
        }
        if self.default_cooldown_ms > self.max_cooldown_ms {
            return Err(MoaError::ConfigError(
                "providers.pacing.default_cooldown_ms must not exceed max_cooldown_ms".to_string(),
            ));
        }
        if self.max_pacing_wait_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.pacing.max_pacing_wait_ms must be greater than zero".to_string(),
            ));
        }
        if self.is_global() && self.state_ttl_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.pacing.state_ttl_ms must be greater than zero for global scope"
                    .to_string(),
            ));
        }
        Ok(())
    }
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
    /// What every distributed provider control does when the coordination store
    /// is unreachable or was never injected. Applies to concurrency admission and
    /// to pacing, cooldown, and retry budget alike, so one deployment decision
    /// governs the whole fleet-coordination surface.
    pub on_coordination_failure: CoordinationFailurePolicy,
}

impl Default for ProviderConcurrencyConfig {
    fn default() -> Self {
        Self {
            scope: ConcurrencyScope::Local,
            default_max_in_flight: 16,
            block_threshold_ms: 2_000,
            lease_ttl_ms: 600_000,
            on_coordination_failure: CoordinationFailurePolicy::BoundedDegraded,
        }
    }
}

impl ProviderConcurrencyConfig {
    /// Returns whether in-flight admission is coordinated across replicas.
    #[must_use]
    pub const fn is_global(&self) -> bool {
        matches!(self.scope, ConcurrencyScope::Global)
    }

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

/// Deadlines applied while consuming one streaming LLM response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderStreamTimeoutConfig {
    /// Maximum wait for the first server-sent event, in milliseconds.
    pub first_byte_ms: u64,
    /// Maximum idle gap between server-sent events, in milliseconds.
    pub idle_ms: u64,
    /// Maximum wall-clock duration of the complete stream, in milliseconds.
    pub total_ms: u64,
}

impl Default for ProviderStreamTimeoutConfig {
    fn default() -> Self {
        Self {
            first_byte_ms: 30_000,
            idle_ms: 60_000,
            total_ms: 300_000,
        }
    }
}

impl ProviderStreamTimeoutConfig {
    /// Validates that every streaming deadline is positive and fits within the total deadline.
    pub fn validate(&self) -> Result<(), MoaError> {
        if self.first_byte_ms == 0 || self.idle_ms == 0 || self.total_ms == 0 {
            return Err(MoaError::ConfigError(
                "providers.stream_timeouts values must be greater than zero".to_string(),
            ));
        }
        if self.first_byte_ms > self.total_ms || self.idle_ms > self.total_ms {
            return Err(MoaError::ConfigError(
                "providers.stream_timeouts first_byte_ms and idle_ms must not exceed total_ms"
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
///
/// Deployment capability assertions are grouped under
/// [`capabilities`](Self::capabilities), separate from credentials and pacing.
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
    /// Operator-asserted deployment capabilities for this provider endpoint.
    #[serde(
        default,
        skip_serializing_if = "ProviderCapabilitiesConfig::is_conservative"
    )]
    pub capabilities: ProviderCapabilitiesConfig,
}

/// Serde predicate: skip serializing a conservative-`false` capability flag so a
/// bare credential still round-trips to `{ "api_key": ... }`.
fn is_conservative_false(value: &bool) -> bool {
    !*value
}

impl ProviderCredentialConfig {
    /// Creates a provider credential config with a direct API key value.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            max_requests_per_min: None,
            max_inputs_per_min: None,
            max_concurrent_requests: None,
            capabilities: ProviderCapabilitiesConfig::default(),
        }
    }
}

impl Default for ProviderCredentialConfig {
    fn default() -> Self {
        Self::new("")
    }
}

/// Operator-asserted capabilities of one configured provider endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCapabilitiesConfig {
    /// Whether the endpoint guarantees no training on or retention of requests.
    #[serde(default, skip_serializing_if = "is_conservative_false")]
    pub zero_retention: bool,
    /// Whether the endpoint is a private or self-hosted deployment.
    #[serde(default, skip_serializing_if = "is_conservative_false")]
    pub private_deployment: bool,
    /// Contractual data-residency region, or `None` when none is asserted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_residency: Option<String>,
}

impl ProviderCapabilitiesConfig {
    fn is_conservative(&self) -> bool {
        !self.zero_retention && !self.private_deployment && self.data_residency.is_none()
    }

    fn validate(&self, provider: ProviderId) -> Result<(), MoaError> {
        if self
            .data_residency
            .as_deref()
            .is_some_and(|residency| residency.trim().is_empty())
        {
            return Err(MoaError::ConfigError(format!(
                "providers.{provider}.capabilities.data_residency must be non-empty when set"
            )));
        }
        Ok(())
    }
}

/// Deployment-wide provider-routing policy applied to every completion.
///
/// The runtime owns one provider registry for the deployment. This policy
/// therefore constrains every tenant served by that registry; tenant-specific
/// policy belongs at a request-scoped routing boundary, not in process config.
///
/// Every field is off by default, so a deployment with no policy routes exactly
/// as before with zero added overhead. The policy is
/// [`is_active`](Self::is_active) only when at least one constraint is set.
///
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeploymentProviderPolicyConfig {
    /// Require the selected provider to assert zero request retention.
    #[serde(default, skip_serializing_if = "is_conservative_false")]
    pub require_zero_retention: bool,
    /// Require the selected provider to assert a private/self-hosted deployment.
    #[serde(default, skip_serializing_if = "is_conservative_false")]
    pub require_private_deployment: bool,
    /// Allowlist of provider ids. Empty means "no allowlist" (all providers may
    /// be considered, subject to the other constraints); non-empty restricts
    /// routing to exactly these providers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_providers: Vec<ProviderId>,
    /// Denylist of provider ids that must never serve this deployment, regardless of
    /// any other constraint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied_providers: Vec<ProviderId>,
    /// Require the selected provider to assert this data-residency class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_residency: Option<String>,
}

impl DeploymentProviderPolicyConfig {
    /// Returns whether any deployment constraint is set. When `false`, routing is
    /// unchanged and pays zero overhead.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.require_zero_retention
            || self.require_private_deployment
            || !self.allowed_providers.is_empty()
            || !self.denied_providers.is_empty()
            || self.required_residency.is_some()
    }

    /// Validates residency and rejects contradictory allow/deny entries.
    pub fn validate(&self) -> Result<(), MoaError> {
        if self
            .required_residency
            .as_deref()
            .is_some_and(|residency| residency.trim().is_empty())
        {
            return Err(MoaError::ConfigError(
                "providers.routing_policy.required_residency must be non-empty when set"
                    .to_string(),
            ));
        }
        if let Some(provider) = self
            .allowed_providers
            .iter()
            .find(|provider| self.denied_providers.contains(provider))
        {
            return Err(MoaError::ConfigError(format!(
                "providers.routing_policy cannot both allow and deny provider '{provider}'"
            )));
        }
        Ok(())
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
    /// Fleet coordination for per-minute pacing, 429 cooldown, and retry budget.
    #[serde(default)]
    pub pacing: ProviderPacingConfig,
    /// First-byte, idle, and total deadlines for streaming LLM responses.
    #[serde(default)]
    pub stream_timeouts: ProviderStreamTimeoutConfig,
    /// Deployment-wide provider-routing policy. Inactive by default.
    #[serde(default)]
    pub routing_policy: DeploymentProviderPolicyConfig,
}

impl ProvidersConfig {
    /// Validates provider concurrency, stream timeouts, capabilities, and policy.
    pub fn validate(&self) -> Result<(), MoaError> {
        self.concurrency.validate()?;
        self.pacing.validate()?;
        self.stream_timeouts.validate()?;
        if self.concurrency.scope == ConcurrencyScope::Global
            && self.concurrency.lease_ttl_ms <= self.stream_timeouts.total_ms
        {
            return Err(MoaError::ConfigError(
                "providers.concurrency.lease_ttl_ms must exceed providers.stream_timeouts.total_ms under global scope"
                    .to_string(),
            ));
        }
        self.anthropic
            .capabilities
            .validate(ProviderId::Anthropic)?;
        self.openai.capabilities.validate(ProviderId::OpenAI)?;
        self.google.capabilities.validate(ProviderId::Google)?;
        self.routing_policy.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::MoaError;
    use moa_core::types::provider::ProviderId;

    use super::{
        ConcurrencyScope, CoordinationFailurePolicy, DeploymentProviderPolicyConfig,
        ProviderConcurrencyConfig, ProviderCredentialConfig, ProviderPacingConfig,
        ProviderStreamTimeoutConfig, ProvidersConfig,
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
    fn provider_stream_timeouts_are_bounded_and_fit_the_global_lease() {
        // Pins: production streams cannot hold provider permits indefinitely,
        // and a global lease cannot expire while a valid stream is still running.
        let defaults = ProvidersConfig::default();
        assert_eq!(
            defaults.stream_timeouts,
            ProviderStreamTimeoutConfig {
                first_byte_ms: 30_000,
                idle_ms: 60_000,
                total_ms: 300_000,
            }
        );
        assert!(defaults.validate().is_ok());

        let invalid = ProvidersConfig {
            concurrency: ProviderConcurrencyConfig {
                scope: ConcurrencyScope::Global,
                lease_ttl_ms: 300_000,
                ..ProviderConcurrencyConfig::default()
            },
            stream_timeouts: ProviderStreamTimeoutConfig {
                total_ms: 300_000,
                ..ProviderStreamTimeoutConfig::default()
            },
            ..ProvidersConfig::default()
        };
        assert!(
            matches!(invalid.validate(), Err(MoaError::ConfigError(message)) if message.contains("lease_ttl_ms"))
        );
    }

    #[test]
    fn provider_concurrency_config_parses_global_scope_and_rejects_bad_durations() {
        // Pins: operators opt into global coordination via config; a zero block
        // threshold and a zero global lease TTL are rejected.
        let parsed: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "concurrency": { "scope": "global", "default_max_in_flight": 64, "lease_ttl_ms": 600000 }
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
    fn pacing_defaults_reproduce_the_historical_process_local_behavior() {
        // Pins: adding fleet coordination changes nothing for an existing
        // deployment — pacing stays process-local, and the cooldown and
        // retry-budget numbers are the constants the guard used before they
        // became configurable.
        let pacing = ProviderPacingConfig::default();
        assert_eq!(pacing.scope, ConcurrencyScope::Local);
        assert!(!pacing.is_global());
        assert_eq!(pacing.default_cooldown_ms, 5_000);
        assert_eq!(pacing.retry_budget_window_ms, 60_000);
        assert_eq!(pacing.retry_budget_percent, 20);
        assert_eq!(pacing.retry_budget_floor, 8);
        assert!(pacing.validate().is_ok());

        assert_eq!(
            ProviderConcurrencyConfig::default().on_coordination_failure,
            CoordinationFailurePolicy::BoundedDegraded,
            "the default policy must preserve availability, as the code did before"
        );
        assert!(!CoordinationFailurePolicy::BoundedDegraded.rejects_admission());
        assert!(CoordinationFailurePolicy::FailClosed.rejects_admission());
    }

    #[test]
    fn pacing_validation_rejects_budgets_that_cannot_bound_anything() {
        // Pins: each pacing knob that could silently disable a bound is rejected
        // at config load rather than producing an unbounded control at runtime.
        let base = ProviderPacingConfig::default();
        for (label, invalid) in [
            (
                "zero percent",
                ProviderPacingConfig {
                    retry_budget_percent: 0,
                    ..base.clone()
                },
            ),
            (
                "percent above 100",
                ProviderPacingConfig {
                    retry_budget_percent: 101,
                    ..base.clone()
                },
            ),
            (
                "zero window",
                ProviderPacingConfig {
                    retry_budget_window_ms: 0,
                    ..base.clone()
                },
            ),
            (
                "zero pacing wait",
                ProviderPacingConfig {
                    max_pacing_wait_ms: 0,
                    ..base.clone()
                },
            ),
            (
                "cooldown above its own cap",
                ProviderPacingConfig {
                    default_cooldown_ms: 10_000,
                    max_cooldown_ms: 5_000,
                    ..base.clone()
                },
            ),
            (
                "global scope with no state ttl",
                ProviderPacingConfig {
                    scope: ConcurrencyScope::Global,
                    state_ttl_ms: 0,
                    ..base.clone()
                },
            ),
        ] {
            assert!(
                invalid.validate().is_err(),
                "{label} must be rejected by pacing validation"
            );
        }

        // A zero state TTL only matters when the state is shared.
        assert!(
            ProviderPacingConfig {
                state_ttl_ms: 0,
                ..base
            }
            .validate()
            .is_ok(),
            "a process-local deployment has no shared state to expire"
        );
    }

    #[test]
    fn coordination_policy_and_pacing_scope_parse_from_config() {
        // Pins: operators opt into fleet-wide pacing and the strict failure policy
        // through config, and `ProvidersConfig::validate` reaches the pacing
        // section (an invalid pacing block fails the whole provider config).
        let parsed: ProvidersConfig = serde_json::from_value(serde_json::json!({
            "concurrency": { "on_coordination_failure": "fail_closed" },
            "pacing": { "scope": "global", "retry_budget_percent": 50 }
        }))
        .expect("providers config with coordination settings should parse");
        assert_eq!(
            parsed.concurrency.on_coordination_failure,
            CoordinationFailurePolicy::FailClosed
        );
        assert!(parsed.pacing.is_global());
        assert_eq!(parsed.pacing.retry_budget_percent, 50);
        assert!(parsed.validate().is_ok());

        let invalid = ProvidersConfig {
            pacing: ProviderPacingConfig {
                retry_budget_percent: 0,
                ..ProviderPacingConfig::default()
            },
            ..ProvidersConfig::default()
        };
        assert!(
            invalid.validate().is_err(),
            "provider validation must reach the pacing section"
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

    #[test]
    fn provider_capabilities_default_conservative_and_are_omitted() {
        // Pins: capability assertions default to the conservative value so a bare
        // credential neither over-claims compliance nor changes on the wire, and
        // an operator can assert compliance explicitly.
        let defaulted = ProviderCredentialConfig::new("only-key");
        assert!(!defaulted.capabilities.zero_retention);
        assert!(!defaulted.capabilities.private_deployment);
        assert_eq!(defaulted.capabilities.data_residency, None);

        let serialized = serde_json::to_value(&defaulted).expect("serialize");
        assert_eq!(serialized, serde_json::json!({ "api_key": "only-key" }));

        let asserted: ProviderCredentialConfig = serde_json::from_value(serde_json::json!({
            "api_key": "zdr-key",
            "capabilities": {
                "zero_retention": true,
                "private_deployment": true,
                "data_residency": "eu"
            }
        }))
        .expect("credential with capability assertions should parse");
        assert!(asserted.capabilities.zero_retention);
        assert!(asserted.capabilities.private_deployment);
        assert_eq!(asserted.capabilities.data_residency.as_deref(), Some("eu"));
    }

    #[test]
    fn deployment_policy_is_active_only_when_a_constraint_is_set() {
        // Pins: an empty policy is inactive (zero-overhead routing), and each
        // individual constraint flips it active.
        assert!(!DeploymentProviderPolicyConfig::default().is_active());
        assert!(
            DeploymentProviderPolicyConfig {
                require_zero_retention: true,
                ..DeploymentProviderPolicyConfig::default()
            }
            .is_active()
        );
        assert!(
            DeploymentProviderPolicyConfig {
                allowed_providers: vec![ProviderId::Anthropic],
                ..DeploymentProviderPolicyConfig::default()
            }
            .is_active()
        );
        assert!(
            DeploymentProviderPolicyConfig {
                required_residency: Some("eu".to_string()),
                ..DeploymentProviderPolicyConfig::default()
            }
            .is_active()
        );
    }

    #[test]
    fn deployment_policy_rejects_unknown_and_contradictory_provider_ids() {
        // Pins: typed deployment policy rejects unknown ids during parsing and
        // cannot simultaneously allow and deny one provider.
        let good = DeploymentProviderPolicyConfig {
            allowed_providers: vec![ProviderId::Anthropic],
            denied_providers: vec![ProviderId::OpenAI],
            ..DeploymentProviderPolicyConfig::default()
        };
        assert!(good.validate().is_ok());

        let error = serde_json::from_value::<DeploymentProviderPolicyConfig>(
            serde_json::json!({ "allowed_providers": ["anthropicc"] }),
        )
        .expect_err("unknown provider name must be rejected during parsing");
        assert!(error.to_string().contains("anthropicc"), "{error}");

        let overlap = DeploymentProviderPolicyConfig {
            allowed_providers: vec![ProviderId::Anthropic],
            denied_providers: vec![ProviderId::Anthropic],
            ..DeploymentProviderPolicyConfig::default()
        };
        let error = overlap
            .validate()
            .expect_err("overlapping allow and deny entries must be rejected");
        assert!(
            error.to_string().contains("anthropic"),
            "error names the contradictory provider: {error}"
        );
    }
}
