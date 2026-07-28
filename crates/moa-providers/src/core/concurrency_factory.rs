//! Fleet coordination for the provider controls that share one API-key quota.
//!
//! Provider rate limits are tied to the account tier, so every control here is
//! scoped to a **quota identity**: the provider, the opaque fingerprint of the
//! credential, and — for the per-minute controls — the model and rate class. One
//! credential's in-flight budget is shared across every call kind it serves (e.g.
//! Cohere embed + rerank on one key); its per-minute pacing, 429 cooldown, and
//! retry budget are shared per model and rate class.
//!
//! # Local versus global
//!
//! Under [`ConcurrencyScope::Local`] each control is a process-local bound: one
//! shared semaphore or token bucket per budget key inside this process. That is a
//! deliberate deployment choice for a single node, and it is silent.
//!
//! Under [`ConcurrencyScope::Global`] the control is enforced once for the whole
//! fleet through bounded atomic operations on the runtime coordination store, so
//! an autoscaled deployment does not multiply one API key's documented budget by
//! its replica count.
//!
//! # Where the store comes from
//!
//! [`ProviderCoordination::from_config`] reads the store that the composition
//! root injected into [`MoaConfig::with_runtime_coordination`]. There is no
//! process-global handle and therefore no install-ordering hazard: a config value
//! either carries the store or it does not, and a deployment that declares a
//! distributed scope without one is a *coordination failure* subject to the
//! configured [`CoordinationFailurePolicy`] — never a silent downgrade to a
//! per-process ceiling.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use moa_config::MoaConfig;
use moa_config::{
    ConcurrencyScope, CoordinationFailurePolicy, ProviderConcurrencyConfig, ProviderPacingConfig,
};
use moa_core::traits::RuntimeCacheStore;
use moa_core::{error::MoaError, error::Result};
use tokio::sync::Semaphore;

use super::concurrency::ConcurrencyLimiter;
use super::global_concurrency::GlobalConcurrency;
use super::pacer::{PacerConfig, RatePacer};
use super::rate_guard::RateGuard;

/// The kind of provider call a limiter guards. The budget is shared per
/// (provider, credential) regardless of kind; this is used only as a metrics
/// label so per-kind traffic through the shared budget stays observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallKind {
    /// Chat/LLM completion calls.
    Chat,
    /// Embedding calls.
    Embedding,
    /// Rerank calls.
    Rerank,
}

impl CallKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
        }
    }
}

/// Which distributed control a coordination outcome belongs to, for metrics and
/// operator-facing warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoordinatedControl {
    /// Construction time: a distributed scope was configured with no store.
    Startup,
    /// In-flight admission leases.
    Concurrency,
    /// Per-minute request/input token buckets.
    Pacing,
    /// Post-429 cooldown deadline.
    Cooldown,
    /// In-call retry budget.
    RetryBudget,
}

impl CoordinatedControl {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Concurrency => "concurrency",
            Self::Pacing => "pacing",
            Self::Cooldown => "cooldown",
            Self::RetryBudget => "retry_budget",
        }
    }
}

/// Process-local budgets: one shared semaphore per `(provider, credential)` so
/// every call kind on a credential contends for the same in-flight budget.
///
/// This is a registry of process-local semaphores, not coordination state: it
/// exists so two independently-constructed limiters for one credential share a
/// ceiling inside this process. Cross-replica coordination never goes through it.
static LOCAL_BUDGETS: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();

/// Returns the shared local semaphore for one budget key, creating it once.
///
/// The first caller for a key fixes its size; later limiters for the same
/// `(provider, credential)` clone that one semaphore, so kinds share the budget.
fn local_budget_semaphore(key: &str, limit: usize) -> Arc<Semaphore> {
    let registry = LOCAL_BUDGETS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut budgets = registry
        .lock()
        .expect("provider concurrency budget registry mutex poisoned");
    Arc::clone(
        budgets
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(limit))),
    )
}

/// Records that a distributed control fell back to its process-local bound.
///
/// The warning states the consequence in operator terms — while degraded, the
/// effective ceiling is this replica's bound multiplied by the replica count —
/// and the duration says how long the degraded path took.
pub(crate) fn record_coordination_degraded(
    provider: &str,
    control: CoordinatedControl,
    elapsed: Duration,
    error: &MoaError,
) {
    tracing::warn!(
        provider = %provider,
        control = control.label(),
        degraded_ms = elapsed.as_millis(),
        error = %error,
        "provider coordination store unavailable; falling back to this replica's local bound. \
         The effective fleet ceiling is now multiplied by the replica count until it recovers."
    );
    metrics::counter!(
        "moa_provider_coordination_degraded_total",
        "provider" => provider.to_string(),
        "control" => control.label(),
    )
    .increment(1);
    metrics::histogram!(
        "moa_provider_coordination_degraded_duration_seconds",
        "provider" => provider.to_string(),
        "control" => control.label(),
    )
    .record(elapsed.as_secs_f64());
}

/// Records that a distributed control rejected admission rather than enforcing a
/// ceiling that is no longer fleet-wide.
pub(crate) fn record_coordination_rejected(
    provider: &str,
    control: CoordinatedControl,
    error: &MoaError,
) {
    tracing::warn!(
        provider = %provider,
        control = control.label(),
        error = %error,
        "provider coordination store unavailable and the policy is fail_closed; rejecting admission"
    );
    metrics::counter!(
        "moa_provider_coordination_rejected_total",
        "provider" => provider.to_string(),
        "control" => control.label(),
    )
    .increment(1);
}

/// Coordination policy plus the injected runtime store, resolved once.
///
/// `Debug` reports only whether a store is present: the handle itself is a live
/// connection, not data worth rendering.
#[derive(Clone)]
pub(crate) struct ProviderCoordination {
    concurrency: ProviderConcurrencyConfig,
    pacing: ProviderPacingConfig,
    store: Option<Arc<dyn RuntimeCacheStore>>,
}

impl ProviderCoordination {
    /// Resolves coordination from config and the store injected at the
    /// composition root.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a distributed scope is configured, no
    /// coordination store was injected, and the policy is
    /// [`CoordinationFailurePolicy::FailClosed`].
    pub(crate) fn from_config(config: &MoaConfig) -> Result<Self> {
        Self::new(
            config.providers.concurrency.clone(),
            config.providers.pacing.clone(),
            config.runtime_coordination.store(),
        )
    }

    /// Builds coordination from explicit policy and an optional store.
    pub(crate) fn new(
        concurrency: ProviderConcurrencyConfig,
        pacing: ProviderPacingConfig,
        store: Option<Arc<dyn RuntimeCacheStore>>,
    ) -> Result<Self> {
        if store.is_none() && (concurrency.is_global() || pacing.is_global()) {
            let error = MoaError::ConfigError(
                "providers coordination declares a global scope but no runtime coordination \
                 store was injected; pass one through MoaConfig::with_runtime_coordination or \
                 set the scope to 'local'"
                    .to_string(),
            );
            if concurrency.on_coordination_failure.rejects_admission() {
                return Err(error);
            }
            // A missing store degrades every provider this process builds, so the
            // startup signal is not attributable to one provider label.
            record_coordination_degraded(
                "any",
                CoordinatedControl::Startup,
                Duration::ZERO,
                &error,
            );
        }
        Ok(Self {
            concurrency,
            pacing,
            store,
        })
    }

    /// Returns the store to coordinate `control` through, or `None` when this
    /// control is deliberately process-local.
    fn coordinated_store(&self, control: CoordinatedControl) -> Option<Arc<dyn RuntimeCacheStore>> {
        let scope = match control {
            CoordinatedControl::Concurrency => self.concurrency.scope,
            _ => self.pacing.scope,
        };
        (scope == ConcurrencyScope::Global)
            .then(|| self.store.clone())
            .flatten()
    }

    /// Returns the policy applied when a distributed control cannot coordinate.
    pub(crate) fn failure_policy(&self) -> CoordinationFailurePolicy {
        self.concurrency.on_coordination_failure
    }

    /// Builds the shared limiter for one `(provider, credential)` budget.
    ///
    /// The effective limit is the provider's `max_concurrent_requests` when set,
    /// else the workspace `default_max_in_flight` (`0` = unbounded). Global scope
    /// with a positive limit and an injected store yields a cross-replica limiter
    /// keyed on the shared budget; every other case is process-local, sharing one
    /// semaphore per budget key so all call kinds on the credential contend for
    /// the same ceiling.
    pub(crate) fn limiter(
        &self,
        kind: CallKind,
        provider: &str,
        credential: &str,
        per_provider_override: Option<u32>,
    ) -> ConcurrencyLimiter {
        let limit =
            per_provider_override.unwrap_or(self.concurrency.default_max_in_flight) as usize;
        let block_threshold = Duration::from_millis(self.concurrency.block_threshold_ms);
        let key = budget_key(provider, credential);

        match self.coordinated_store(CoordinatedControl::Concurrency) {
            Some(store) if limit > 0 => {
                // The degrade-open fallback is the same shared per-(provider,
                // credential) semaphore as the local path, so kinds still share
                // one budget when the coordination store is unavailable.
                let fallback = local_budget_semaphore(&key, limit);
                let global = GlobalConcurrency::new(
                    store,
                    key,
                    limit,
                    Duration::from_millis(self.concurrency.lease_ttl_ms),
                    provider.to_string(),
                    kind.label(),
                    fallback,
                    self.failure_policy(),
                );
                ConcurrencyLimiter::global(global, block_threshold)
            }
            _ => {
                let semaphore = (limit > 0).then(|| local_budget_semaphore(&key, limit));
                ConcurrencyLimiter::from_local_semaphore(semaphore, block_threshold)
            }
        }
    }

    /// Builds the per-minute pacer for one credential's quota.
    ///
    /// Under global pacing scope the token buckets live in the coordination
    /// store, keyed per provider, credential fingerprint, model, and rate class,
    /// so one documented per-minute budget is spent once by the whole fleet.
    pub(crate) fn pacer(&self, config: PacerConfig, provider: &str, credential: &str) -> RatePacer {
        self.share_pacing(RatePacer::new(config), provider, credential)
    }

    /// Attaches this credential's shared quota to an already-built pacer.
    ///
    /// Providers whose per-minute defaults live inside the provider type (the
    /// embedding and rerank clients) hand their effective pacer here, so the
    /// built-in default is coordinated exactly like an operator override.
    pub(crate) fn share_pacing(
        &self,
        pacer: RatePacer,
        provider: &str,
        credential: &str,
    ) -> RatePacer {
        pacer.with_shared_quota(
            self.coordinated_store(CoordinatedControl::Pacing),
            QuotaIdentity::new(provider, credential),
            self.pacing.clone(),
            self.failure_policy(),
        )
    }

    /// Builds the 429-cooldown and retry-budget guard for one credential's quota.
    pub(crate) fn rate_guard(&self, kind: CallKind, provider: &str, credential: &str) -> RateGuard {
        RateGuard::new(self.pacing.clone())
            .with_class(kind.label())
            .with_shared_quota(
                self.coordinated_store(CoordinatedControl::Cooldown),
                QuotaIdentity::new(provider, credential),
                self.failure_policy(),
            )
    }
}

impl std::fmt::Debug for ProviderCoordination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderCoordination")
            .field("concurrency_scope", &self.concurrency.scope)
            .field("pacing_scope", &self.pacing.scope)
            .field("on_coordination_failure", &self.failure_policy())
            .field("store", &self.store.is_some())
            .finish()
    }
}

/// The identity one fleet-shared provider quota is keyed by.
///
/// Holds the provider name and an opaque fingerprint of the credential. The raw
/// key material is hashed on construction and never stored, so nothing derived
/// from it can reach a coordination-store key, a metric label, or a log line.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QuotaIdentity {
    provider: String,
    credential: CredentialFingerprint,
}

impl QuotaIdentity {
    /// Builds a quota identity, fingerprinting the credential immediately.
    pub(crate) fn new(provider: &str, credential: &str) -> Self {
        Self {
            provider: provider.to_string(),
            credential: CredentialFingerprint::of(credential),
        }
    }

    /// Returns the provider name, which is safe to use as a metric label.
    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the coordination-store key for one control, model, and rate class.
    pub(crate) fn key(&self, control: &str, model: &str, class: &str) -> String {
        format!(
            "moa:provider-{control}:{}:{model}:{class}:{}",
            self.provider,
            self.credential.as_str()
        )
    }
}

impl std::fmt::Debug for QuotaIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuotaIdentity")
            .field("provider", &self.provider)
            .field("credential", &self.credential)
            .finish()
    }
}

/// Domain separation label for credential fingerprints, so a fingerprint minted
/// here can never collide with a digest of the same secret taken elsewhere.
const CREDENTIAL_FINGERPRINT_CONTEXT: &str = "moa provider quota credential fingerprint v1";

/// Hex characters kept from the fingerprint digest (16 bytes of BLAKE3).
const CREDENTIAL_FINGERPRINT_HEX_LEN: usize = 32;

/// One provider credential reduced to an opaque, stable identity.
///
/// The API key is passed through a domain-separated BLAKE3 derivation and only
/// the resulting hex digest is retained, so neither `Debug`, a store key, nor a
/// metric label can carry key material. Deterministic, so two replicas holding
/// the same key agree on one quota identity without exchanging it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CredentialFingerprint(String);

impl CredentialFingerprint {
    /// Fingerprints one credential value.
    pub(crate) fn of(credential: &str) -> Self {
        let digest = blake3::derive_key(CREDENTIAL_FINGERPRINT_CONTEXT, credential.as_bytes());
        let mut hex = String::with_capacity(CREDENTIAL_FINGERPRINT_HEX_LEN);
        for byte in digest.iter().take(CREDENTIAL_FINGERPRINT_HEX_LEN / 2) {
            hex.push_str(&format!("{byte:02x}"));
        }
        Self(hex)
    }

    /// Returns the opaque hex identity.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for CredentialFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The stored value is already opaque; printing it cannot leak the key.
        formatter.write_str(&self.0)
    }
}

/// Builds the shared in-flight budget key for one `(provider, credential)`.
///
/// The in-flight budget is per credential and shared across call kinds and
/// models, so this key deliberately carries neither.
fn budget_key(provider: &str, credential: &str) -> String {
    format!(
        "moa:provider-concurrency:{provider}:{}",
        CredentialFingerprint::of(credential).as_str()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use moa_core::error::Result;
    use tokio::sync::Mutex;

    use super::*;

    fn global_concurrency_config() -> ProviderConcurrencyConfig {
        ProviderConcurrencyConfig {
            scope: ConcurrencyScope::Global,
            ..ProviderConcurrencyConfig::default()
        }
    }

    fn coordination(
        concurrency: ProviderConcurrencyConfig,
        store: Option<Arc<dyn RuntimeCacheStore>>,
    ) -> ProviderCoordination {
        ProviderCoordination::new(concurrency, ProviderPacingConfig::default(), store)
            .expect("coordination should build")
    }

    /// Minimal in-memory coordination store for the global-scope factory test.
    #[derive(Default)]
    struct TestStore {
        entries: Mutex<HashMap<String, Vec<u8>>>,
        leases: Mutex<HashMap<String, Vec<String>>>,
    }

    #[async_trait]
    impl RuntimeCacheStore for TestStore {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.entries.lock().await.get(key).cloned())
        }
        async fn set(&self, key: &str, value: Vec<u8>, _ttl: Duration) -> Result<()> {
            self.entries.lock().await.insert(key.to_string(), value);
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.entries.lock().await.remove(key);
            Ok(())
        }
        async fn compare_and_set(
            &self,
            key: &str,
            expected: Option<&[u8]>,
            value: Vec<u8>,
            _ttl: Duration,
        ) -> Result<bool> {
            let mut entries = self.entries.lock().await;
            if entries.get(key).map(|value| value.as_slice()) != expected {
                return Ok(false);
            }
            entries.insert(key.to_string(), value);
            Ok(true)
        }
        async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
            Ok(())
        }
        async fn try_acquire_bounded_lease(
            &self,
            key: &str,
            lease_id: &str,
            limit: usize,
            _ttl: Duration,
        ) -> Result<moa_core::traits::BoundedLeaseDecision> {
            let mut leases = self.leases.lock().await;
            let held = leases.entry(key.to_string()).or_default();
            if held.iter().any(|id| id == lease_id) {
                let live = held.len();
                return Ok(moa_core::traits::BoundedLeaseDecision {
                    acquired: true,
                    live,
                });
            }
            if held.len() >= limit {
                let live = held.len();
                return Ok(moa_core::traits::BoundedLeaseDecision {
                    acquired: false,
                    live,
                });
            }
            held.push(lease_id.to_string());
            let live = held.len();
            Ok(moa_core::traits::BoundedLeaseDecision {
                acquired: true,
                live,
            })
        }
        async fn release_bounded_lease(&self, key: &str, lease_id: &str) -> Result<usize> {
            let mut leases = self.leases.lock().await;
            let Some(held) = leases.get_mut(key) else {
                return Ok(0);
            };
            held.retain(|id| id != lease_id);
            Ok(held.len())
        }
    }

    #[test]
    fn local_scope_builds_a_local_bounded_limiter() {
        // Pins: the default (local) scope yields a bounded process-local limiter
        // sized by the workspace default, ignoring any injected store.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let limiter = policy.limiter(CallKind::Chat, "anthropic", "local-scope-key", None);
        assert!(limiter.is_bounded());
    }

    #[tokio::test]
    async fn unconfigured_provider_uses_the_workspace_fallback_default() {
        // Pins: a provider with no override draws from `default_max_in_flight` — a
        // fallback of 1 bounds the credential to a single in-flight slot.
        let config = ProviderConcurrencyConfig {
            default_max_in_flight: 1,
            ..ProviderConcurrencyConfig::default()
        };
        let policy = coordination(config, None);
        let first = policy.limiter(CallKind::Chat, "openai", "fallback-key", None);
        let second = policy.limiter(CallKind::Chat, "openai", "fallback-key", None);
        let held = first
            .acquire_within(Duration::from_millis(10))
            .await
            .expect("the fallback allows one slot");
        assert!(
            second
                .acquire_within(Duration::from_millis(10))
                .await
                .is_none(),
            "the workspace fallback of 1 must bound the credential to one slot"
        );
        drop(held);
    }

    #[test]
    fn global_scope_without_a_store_degrades_to_local_under_the_default_policy() {
        // Pins: global scope with no injected store is a coordination failure, not
        // a silent downgrade: under the default bounded_degraded policy it still
        // builds a bounded local limiter (availability preserved).
        let policy = coordination(global_concurrency_config(), None);
        let limiter = policy.limiter(
            CallKind::Embedding,
            "openai",
            "global-no-store-key",
            Some(4),
        );
        assert!(limiter.is_bounded());
    }

    #[test]
    fn global_scope_without_a_store_fails_closed_when_configured() {
        // Pins: the missing-store startup case follows the configured policy —
        // fail_closed refuses to construct rather than quietly enforcing a
        // per-process ceiling that the replica count would multiply.
        let error = ProviderCoordination::new(
            ProviderConcurrencyConfig {
                on_coordination_failure: CoordinationFailurePolicy::FailClosed,
                ..global_concurrency_config()
            },
            ProviderPacingConfig::default(),
            None,
        )
        .expect_err("fail_closed must reject a global scope with no coordination store");
        let message = error.to_string();
        assert!(
            message.contains("with_runtime_coordination") && message.contains("local"),
            "the error must tell the operator how to fix it: {message}"
        );
    }

    #[test]
    fn global_pacing_without_a_store_fails_closed_too() {
        // Pins: the policy covers EVERY distributed control, not just concurrency
        // — a global pacing scope with no store is the same startup failure.
        let error = ProviderCoordination::new(
            ProviderConcurrencyConfig {
                on_coordination_failure: CoordinationFailurePolicy::FailClosed,
                ..ProviderConcurrencyConfig::default()
            },
            ProviderPacingConfig {
                scope: ConcurrencyScope::Global,
                ..ProviderPacingConfig::default()
            },
            None,
        )
        .expect_err("a global pacing scope with no store must fail closed");
        assert!(matches!(error, MoaError::ConfigError(_)));
    }

    #[test]
    fn a_deliberate_local_deployment_is_not_a_coordination_failure() {
        // Pins: local scope with no store is configuration, not an error — it
        // builds cleanly even under the strictest failure policy.
        ProviderCoordination::new(
            ProviderConcurrencyConfig {
                on_coordination_failure: CoordinationFailurePolicy::FailClosed,
                ..ProviderConcurrencyConfig::default()
            },
            ProviderPacingConfig::default(),
            None,
        )
        .expect("a local-only deployment must build without a coordination store");
    }

    #[test]
    fn per_provider_override_of_zero_is_unbounded() {
        // Pins: an explicit 0 override opts back into unbounded, even under global
        // scope (no coordination for an unbounded budget).
        let policy = coordination(global_concurrency_config(), None);
        let limiter = policy.limiter(CallKind::Chat, "anthropic", "unbounded-key", Some(0));
        assert!(!limiter.is_bounded());
    }

    #[tokio::test]
    async fn call_kinds_on_one_credential_share_a_local_budget() {
        // Pins: limiters built for the same (provider, credential) — the embed,
        // rerank, and chat clients on one Cohere key — contend for one shared
        // process-local budget; a different provider has an independent budget.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let embed = policy.limiter(CallKind::Embedding, "cohere", "local-shared-key", Some(1));
        let rerank = policy.limiter(CallKind::Rerank, "cohere", "local-shared-key", Some(1));
        let chat = policy.limiter(CallKind::Chat, "cohere", "local-shared-key", Some(1));

        let held = embed
            .acquire_within(Duration::from_millis(10))
            .await
            .expect("embed takes the only shared slot");
        assert!(
            rerank
                .acquire_within(Duration::from_millis(10))
                .await
                .is_none(),
            "rerank on the same credential must find the shared budget saturated"
        );
        assert!(
            chat.acquire_within(Duration::from_millis(10))
                .await
                .is_none(),
            "chat on the same credential must find the shared budget saturated"
        );

        // A different provider draws from an independent budget.
        let other = policy.limiter(CallKind::Chat, "openai", "local-shared-key", Some(1));
        assert!(
            other
                .acquire_within(Duration::from_millis(10))
                .await
                .is_some(),
            "a different provider has its own budget"
        );
        drop(held);
    }

    #[tokio::test]
    async fn call_kinds_on_one_credential_share_a_global_budget() {
        // Pins: under global scope, kinds on one credential share one lease budget
        // in the coordination store (same key, no call kind) — embed holding the
        // only slot saturates rerank on the same credential.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(TestStore::default());
        let policy = coordination(global_concurrency_config(), Some(store));
        let embed = policy.limiter(CallKind::Embedding, "cohere", "global-shared-key", Some(1));
        let rerank = policy.limiter(CallKind::Rerank, "cohere", "global-shared-key", Some(1));

        let held = embed
            .acquire_within(Duration::from_millis(50))
            .await
            .expect("embed takes the only global lease");
        assert!(
            rerank
                .acquire_within(Duration::from_millis(50))
                .await
                .is_none(),
            "rerank must find the shared global lease budget saturated"
        );
        drop(held);
    }

    #[test]
    fn quota_keys_are_stable_opaque_and_separated_by_every_dimension() {
        // Pins: a quota key is deterministic for one (provider, credential, model,
        // rate class), differs when any dimension differs, and never contains the
        // raw credential — the fingerprint is all that reaches the store.
        let cohere = QuotaIdentity::new("cohere", "secret-key");
        let same = QuotaIdentity::new("cohere", "secret-key");
        let other_key = QuotaIdentity::new("cohere", "other-key");
        let other_provider = QuotaIdentity::new("openai", "secret-key");

        let base = cohere.key("pace", "embed-v4.0", "inputs");
        assert_eq!(base, same.key("pace", "embed-v4.0", "inputs"));
        assert_ne!(base, other_key.key("pace", "embed-v4.0", "inputs"));
        assert_ne!(base, other_provider.key("pace", "embed-v4.0", "inputs"));
        assert_ne!(base, cohere.key("pace", "embed-v3.0", "inputs"));
        assert_ne!(base, cohere.key("pace", "embed-v4.0", "requests"));
        assert_ne!(base, cohere.key("cooldown", "embed-v4.0", "inputs"));

        assert!(
            !base.contains("secret-key"),
            "raw key material must never reach a store key"
        );
        assert!(
            !format!("{cohere:?}").contains("secret-key"),
            "raw key material must never reach a debug rendering"
        );
        assert_eq!(
            CredentialFingerprint::of("secret-key").as_str().len(),
            CREDENTIAL_FINGERPRINT_HEX_LEN
        );
    }

    #[test]
    fn in_flight_budget_key_hides_the_credential_and_ignores_model_and_kind() {
        // Pins: the in-flight budget is per (provider, credential) only, so every
        // call kind and model on one key contends for the same ceiling, and the
        // raw key never appears in the coordination-store key.
        let a = budget_key("cohere", "secret-key");
        assert_eq!(a, budget_key("cohere", "secret-key"));
        assert_ne!(a, budget_key("cohere", "other-key"));
        assert_ne!(a, budget_key("openai", "secret-key"));
        assert!(!a.contains("secret-key"));
        assert!(a.starts_with("moa:provider-concurrency:cohere:"));
    }
}
