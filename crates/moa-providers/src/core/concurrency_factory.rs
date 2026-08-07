//! Fleet coordination for the provider controls that share one API-key quota.
//!
//! Provider rate limits are tied to the account tier, so every control here is
//! scoped to a **quota identity**: the provider, the opaque fingerprint of the
//! credential, and — for the per-minute controls — the model and rate class. One
//! credential's in-flight budget is shared across every call kind it serves (e.g.
//! Cohere embed + rerank on one key); cooldown and retry budget are shared across
//! every model client on one credential and rate class.
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
//! The composition root passes the runtime store explicitly when it constructs
//! [`ProviderCoordination`]. Serializable configuration never carries a live
//! store handle, and there is no process-global install ordering.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use moa_config::MoaConfig;
use moa_config::{
    ConcurrencyScope, CoordinationFailurePolicy, ProviderConcurrencyConfig, ProviderPacingConfig,
};
use moa_core::traits::RuntimeCacheStore;
use moa_core::{error::MoaError, error::Result};
use moa_runtime_store::DeadlineRuntimeCacheStore;
use tokio::sync::Semaphore;

use super::concurrency::ConcurrencyLimiter;
use super::global_concurrency::{GlobalConcurrency, GlobalConcurrencyConfig};
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
static LOCAL_BUDGETS: OnceLock<Mutex<LocalBudgetRegistry>> = OnceLock::new();

/// Dead process-local semaphore entries inspected on one new-key lookup.
const LOCAL_BUDGET_RECLAIM_BATCH: usize = 8;

/// Cache-only rate guards inspected on one new-key lookup.
const RATE_GUARD_RECLAIM_BATCH: usize = 8;

#[derive(Default)]
struct LocalBudgetRegistry {
    entries: HashMap<String, Weak<Semaphore>>,
    cleanup_order: VecDeque<String>,
    cleanup_cursor: usize,
    #[cfg(test)]
    cleanup_inspections: usize,
}

impl LocalBudgetRegistry {
    fn semaphore(&mut self, key: &str, limit: usize) -> Arc<Semaphore> {
        if let Some(semaphore) = self.entries.get(key).and_then(Weak::upgrade) {
            return semaphore;
        }
        if self.entries.contains_key(key) {
            self.remove(key);
        }

        self.reclaim_dead(LOCAL_BUDGET_RECLAIM_BATCH);
        let semaphore = Arc::new(Semaphore::new(limit));
        self.entries
            .insert(key.to_string(), Arc::downgrade(&semaphore));
        self.cleanup_order.push_back(key.to_string());
        semaphore
    }

    fn reclaim_dead(&mut self, budget: usize) {
        let inspections = budget.min(self.cleanup_order.len());
        for _ in 0..inspections {
            if self.cleanup_order.is_empty() {
                self.cleanup_cursor = 0;
                return;
            }
            if self.cleanup_cursor >= self.cleanup_order.len() {
                self.cleanup_cursor = 0;
            }
            #[cfg(test)]
            {
                self.cleanup_inspections += 1;
            }
            let Some(key) = self.cleanup_order.get(self.cleanup_cursor).cloned() else {
                return;
            };
            let dead = self
                .entries
                .get(&key)
                .is_none_or(|semaphore| semaphore.strong_count() == 0);
            if dead {
                self.entries.remove(&key);
                self.cleanup_order.remove(self.cleanup_cursor);
            } else {
                self.cleanup_cursor += 1;
            }
        }
        if self.cleanup_cursor >= self.cleanup_order.len() {
            self.cleanup_cursor = 0;
        }
    }

    fn remove(&mut self, key: &str) {
        self.entries.remove(key);
        if let Some(position) = self
            .cleanup_order
            .iter()
            .position(|candidate| candidate == key)
        {
            self.cleanup_order.remove(position);
            if position < self.cleanup_cursor {
                self.cleanup_cursor -= 1;
            }
            if self.cleanup_cursor >= self.cleanup_order.len() {
                self.cleanup_cursor = 0;
            }
        }
    }
}

#[derive(Default)]
struct RateGuardCache {
    entries: HashMap<String, RateGuard>,
    cleanup_order: VecDeque<String>,
    cleanup_cursor: usize,
    #[cfg(test)]
    cleanup_inspections: usize,
}

impl RateGuardCache {
    fn get(&self, key: &str) -> Option<RateGuard> {
        self.entries.get(key).cloned()
    }

    fn get_or_insert_with(&mut self, key: String, build: impl FnOnce() -> RateGuard) -> RateGuard {
        if let Some(guard) = self.get(&key) {
            return guard;
        }

        self.reclaim_stale(RATE_GUARD_RECLAIM_BATCH);
        let guard = build();
        self.entries.insert(key.clone(), guard.clone());
        self.cleanup_order.push_back(key);
        guard
    }

    fn reclaim_stale(&mut self, budget: usize) {
        let inspections = budget.min(self.cleanup_order.len());
        for _ in 0..inspections {
            if self.cleanup_order.is_empty() {
                self.cleanup_cursor = 0;
                return;
            }
            if self.cleanup_cursor >= self.cleanup_order.len() {
                self.cleanup_cursor = 0;
            }
            #[cfg(test)]
            {
                self.cleanup_inspections += 1;
            }
            let Some(key) = self.cleanup_order.get(self.cleanup_cursor).cloned() else {
                return;
            };
            let reclaimable = self.entries.get(&key).is_none_or(RateGuard::is_reclaimable);
            if reclaimable {
                self.entries.remove(&key);
                self.cleanup_order.remove(self.cleanup_cursor);
            } else {
                self.cleanup_cursor += 1;
            }
        }
        if self.cleanup_cursor >= self.cleanup_order.len() {
            self.cleanup_cursor = 0;
        }
    }
}

/// Redis coordination commands should be short atomic operations. A fixed bound
/// ensures the configured failure policy decides a hung store instead of waiting
/// forever, without adding another deployment knob.
const COORDINATION_OPERATION_TIMEOUT: Duration = Duration::from_millis(250);

/// Returns the shared local semaphore for one budget key, creating it once.
///
/// The first caller for a key fixes its size; later limiters for the same
/// `(provider, credential)` clone that one semaphore, so kinds share the budget.
fn local_budget_semaphore(key: &str, limit: usize) -> Arc<Semaphore> {
    let registry = LOCAL_BUDGETS.get_or_init(|| Mutex::new(LocalBudgetRegistry::default()));
    let mut budgets = registry.lock().unwrap_or_else(PoisonError::into_inner);
    budgets.semaphore(key, limit)
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
    guards: Arc<Mutex<RateGuardCache>>,
}

impl ProviderCoordination {
    /// Resolves coordination from config and an explicitly injected store.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when a distributed scope is configured, no
    /// coordination store was injected, and the policy is
    /// [`CoordinationFailurePolicy::FailClosed`].
    pub(crate) fn from_config(
        config: &MoaConfig,
        store: Option<Arc<dyn RuntimeCacheStore>>,
    ) -> Result<Self> {
        Self::new(
            config.providers.concurrency.clone(),
            config.providers.pacing.clone(),
            store,
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
                 store was injected at the provider composition root; pass the runtime cache \
                 explicitly or set the scope to 'local'"
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
        let store = store
            .map(|store| {
                DeadlineRuntimeCacheStore::new(store, COORDINATION_OPERATION_TIMEOUT)
                    .map(|store| Arc::new(store) as Arc<dyn RuntimeCacheStore>)
            })
            .transpose()?;
        Ok(Self {
            concurrency,
            pacing,
            store,
            guards: Arc::new(Mutex::new(RateGuardCache::default())),
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
                let global = GlobalConcurrency::new(GlobalConcurrencyConfig {
                    store,
                    quota_key: key,
                    limit,
                    lease_ttl: Duration::from_millis(self.concurrency.lease_ttl_ms),
                    provider_label: provider.to_string(),
                    call_kind_label: kind.label(),
                    local_fallback: fallback,
                    failure_policy: self.failure_policy(),
                });
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
        let identity = QuotaIdentity::new(provider, credential);
        let key = identity.guard_cache_key(kind.label());
        let mut guards = self.guards.lock().unwrap_or_else(PoisonError::into_inner);
        guards.get_or_insert_with(key, || {
            RateGuard::new(self.pacing.clone())
                .with_class(kind.label())
                .with_shared_quota(
                    self.coordinated_store(CoordinatedControl::Cooldown),
                    identity,
                    self.failure_policy(),
                )
        })
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

    /// Returns the fleet pacing key for one model and rate class.
    pub(crate) fn pacing_key(&self, model: &str, class: &str) -> String {
        format!(
            "moa:provider-pacing:{}:{model}:{class}:{}",
            self.provider,
            self.credential.as_str()
        )
    }

    /// Returns the credential cooldown key for one call class.
    pub(crate) fn cooldown_key(&self, class: &str) -> String {
        format!(
            "moa:provider-cooldown:{}:{class}:{}",
            self.provider,
            self.credential.as_str()
        )
    }

    /// Returns the credential retry-budget key for one call class.
    pub(crate) fn retry_budget_key(&self, class: &str) -> String {
        format!(
            "moa:provider-retry-budget:{}:{class}:{}",
            self.provider,
            self.credential.as_str()
        )
    }

    /// Returns the process-local guard-cache key for one call class.
    pub(crate) fn guard_cache_key(&self, class: &str) -> String {
        format!(
            "moa:provider-guard-cache:{}:{class}:{}",
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
        let hex = blake3::Hash::from_bytes(digest).to_hex();
        Self(hex.as_str()[..CREDENTIAL_FINGERPRINT_HEX_LEN].to_owned())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use moa_core::error::Result;
    use tokio::sync::Mutex;

    use crate::core::coordination_test_support::HangingStore;

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

    static TEST_KEY_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn test_budget_key(label: &str) -> String {
        let sequence = TEST_KEY_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("moa:test:{label}:{sequence}")
    }

    fn guard_cache_len(coordination: &ProviderCoordination) -> usize {
        coordination
            .guards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }

    fn guard_cache_contains(coordination: &ProviderCoordination, key: &str) -> bool {
        coordination
            .guards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .contains_key(key)
    }

    fn guard_cleanup_inspections(coordination: &ProviderCoordination) -> usize {
        coordination
            .guards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cleanup_inspections
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

    #[test]
    fn poisoned_local_budget_registry_recovers_without_panicking() {
        // Pins: a poisoned process-local registry remains usable and does not
        // turn a recoverable mutex failure into a library panic.
        let registry =
            LOCAL_BUDGETS.get_or_init(|| std::sync::Mutex::new(LocalBudgetRegistry::default()));
        let poison = std::thread::spawn(move || {
            let _guard = registry.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("intentional provider-budget mutex poison");
        });
        assert!(poison.join().is_err(), "the poison thread must panic");

        let key = test_budget_key("poison-recovery");
        let result = std::panic::catch_unwind(|| local_budget_semaphore(&key, 1));
        assert!(
            result.is_ok(),
            "poison recovery must not panic inside the library helper"
        );
        drop(result.expect("poisoned registry should return a semaphore"));
    }

    #[test]
    fn process_local_semaphore_cleanup_is_amortized_and_skipped_on_live_hits() {
        // Pins: an existing local budget returns the first live semaphore and
        // limit without scanning dead peers; each new key performs only one
        // bounded cleanup batch, and repeated misses eventually reclaim old keys.
        const DISTINCT_KEYS: usize = LOCAL_BUDGET_RECLAIM_BATCH * 4;
        let mut registry = LocalBudgetRegistry::default();
        let first = registry.semaphore("existing", 1);
        let mut dead = Vec::with_capacity(DISTINCT_KEYS);
        let mut dead_keys = Vec::with_capacity(DISTINCT_KEYS);
        for index in 0..DISTINCT_KEYS {
            let key = format!("dead-{index}");
            dead_keys.push(key.clone());
            dead.push(registry.semaphore(&key, 1));
        }
        drop(dead);

        let inspections_before_hit = registry.cleanup_inspections;
        let same = registry.semaphore("existing", 99);
        assert!(Arc::ptr_eq(&first, &same));
        assert_eq!(
            registry.cleanup_inspections, inspections_before_hit,
            "a live existing-key lookup must not inspect unrelated weak entries"
        );

        let entries_before_miss = registry.entries.len();
        let inspections_before_miss = registry.cleanup_inspections;
        drop(registry.semaphore("trigger-0", 1));
        assert_eq!(
            registry.cleanup_inspections - inspections_before_miss,
            LOCAL_BUDGET_RECLAIM_BATCH,
            "one miss must inspect exactly the amortized cleanup batch"
        );
        assert!(
            registry.entries.len() > 2 && registry.entries.len() < entries_before_miss + 1,
            "one cleanup batch should reclaim some, but not all, dead entries"
        );

        for index in 1..=DISTINCT_KEYS {
            drop(registry.semaphore(&format!("trigger-{index}"), 1));
        }
        assert!(
            dead_keys
                .iter()
                .all(|key| !registry.entries.contains_key(key)),
            "bounded cleanup passes must eventually visit every dead semaphore"
        );
    }

    #[tokio::test]
    async fn first_live_local_budget_limit_is_shared_across_later_limiters() {
        // Pins: a live credential keeps the first-created semaphore and its limit
        // even when a later client supplies a different configured limit.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let key = test_budget_key("first-created-limit");
        let first = policy.limiter(CallKind::Chat, "openai", &key, Some(1));
        let later = policy.limiter(CallKind::Embedding, "openai", &key, Some(2));

        let held = first
            .acquire_within(Duration::from_millis(10))
            .await
            .expect("the first limiter should acquire the only slot");
        assert!(
            later
                .acquire_within(Duration::from_millis(10))
                .await
                .is_none(),
            "later limiters must share the first live semaphore and limit"
        );
        drop(held);
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
            message.contains("composition root") && message.contains("local"),
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

    #[tokio::test(start_paused = true)]
    async fn model_clients_on_one_credential_share_the_local_cooldown() {
        // Pins: the registry-owned coordination object returns one credential
        // guard to independently constructed model clients. Switching models
        // cannot escape a 429 when Redis is local or degraded.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let first = policy.rate_guard(CallKind::Chat, "anthropic", "shared-key");
        let second = policy.rate_guard(CallKind::Chat, "anthropic", "shared-key");
        let other = policy.rate_guard(CallKind::Chat, "anthropic", "other-key");

        first
            .record_rate_limited(Some(Duration::from_secs(10)))
            .await;

        assert!(
            second
                .pause_remaining()
                .await
                .expect("shared credential cooldown read")
                .is_some(),
            "a second model client must observe the first client's cooldown"
        );
        assert!(
            other
                .pause_remaining()
                .await
                .expect("other credential cooldown read")
                .is_none(),
            "a different credential must keep independent cooldown state"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_guard_construction_preserves_shared_quota_identity() {
        // Pins: concurrent model-client construction for one credential returns
        // guards backed by one local cooldown state, not one state per caller.
        const CALLERS: usize = 8;
        let policy = Arc::new(coordination(ProviderConcurrencyConfig::default(), None));
        let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS));
        let mut handles = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let policy = Arc::clone(&policy);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                policy.rate_guard(CallKind::Chat, "anthropic", "concurrent-key")
            }));
        }

        let mut guards = Vec::with_capacity(CALLERS);
        for handle in handles {
            guards.push(handle.await.expect("guard construction should not panic"));
        }

        guards[0]
            .record_rate_limited(Some(Duration::from_secs(10)))
            .await;
        for guard in guards.iter().skip(1) {
            assert!(
                guard
                    .pause_remaining()
                    .await
                    .expect("local cooldown read should succeed")
                    .is_some(),
                "all concurrent guard callers must observe one cooldown"
            );
        }
    }

    #[test]
    fn inactive_rate_guards_are_reclaimed_without_evicting_active_clones() {
        // Pins: a guard remains cached while a provider owns a clone, then is
        // reclaimed after every external clone drops.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let active = policy.rate_guard(CallKind::Chat, "anthropic", "active-key");
        let active_clone = policy.rate_guard(CallKind::Chat, "anthropic", "active-key");
        drop(active_clone);

        let other = policy.rate_guard(CallKind::Chat, "anthropic", "other-key");
        assert_eq!(guard_cache_len(&policy), 2);
        drop(active);
        drop(other);

        let replacement = policy.rate_guard(CallKind::Chat, "anthropic", "replacement-key");
        assert_eq!(
            guard_cache_len(&policy),
            1,
            "inactive guard entries must be reclaimed before admitting a new key"
        );
        drop(replacement);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_only_guard_retains_live_cooldown_until_expiry() {
        // Pins: provider-client churn cannot discard a credential cooldown just
        // because the coordination cache owns the guard's final strong handle.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let credential = "cooldown-retention-key";
        let cache_key = QuotaIdentity::new("anthropic", credential).guard_cache_key("chat");
        let guard = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        guard
            .record_rate_limited(Some(Duration::from_secs(10)))
            .await;
        drop(guard);

        drop(policy.rate_guard(CallKind::Chat, "anthropic", "cooldown-trigger"));
        assert!(guard_cache_contains(&policy, &cache_key));
        let reacquired = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        assert!(
            reacquired
                .pause_remaining()
                .await
                .expect("retained cooldown should remain readable")
                .is_some(),
            "a reconstructed model client must observe the live cooldown"
        );

        drop(reacquired);
        tokio::time::advance(Duration::from_secs(11)).await;
        drop(policy.rate_guard(CallKind::Chat, "anthropic", "cooldown-cleanup"));
        assert!(
            !guard_cache_contains(&policy, &cache_key),
            "the cache-only guard should become reclaimable after cooldown expiry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cache_only_guard_retains_retry_budget_until_window_expiry() {
        // Pins: dropping every model client cannot reset an exhausted local
        // retry budget while its measurement window is still live.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let credential = "retry-retention-key";
        let cache_key = QuotaIdentity::new("anthropic", credential).guard_cache_key("chat");
        let guard = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        guard
            .note_request()
            .await
            .expect("request should be counted");
        for _ in 0..ProviderPacingConfig::default().retry_budget_floor {
            assert!(guard.allow_retry().await, "retry floor should be available");
        }
        assert!(
            !guard.allow_retry().await,
            "retry floor should be exhausted"
        );
        drop(guard);

        drop(policy.rate_guard(CallKind::Chat, "anthropic", "retry-trigger"));
        assert!(guard_cache_contains(&policy, &cache_key));
        let reacquired = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        assert!(
            !reacquired.allow_retry().await,
            "a reconstructed model client must inherit the exhausted retry budget"
        );

        drop(reacquired);
        tokio::time::advance(Duration::from_millis(
            ProviderPacingConfig::default().retry_budget_window_ms + 1,
        ))
        .await;
        drop(policy.rate_guard(CallKind::Chat, "anthropic", "retry-cleanup"));
        assert!(
            !guard_cache_contains(&policy, &cache_key),
            "the cache-only guard should become reclaimable after retry-window expiry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn skipped_guard_cleanup_cycles_retain_state_then_rotate_expiry() {
        // Pins: existing-key hits and cleanup batches that have not reached one
        // cache-only guard preserve its live cooldown and exhausted retry budget;
        // a later bounded pass reclaims it only after rotate expires the window.
        let policy = coordination(ProviderConcurrencyConfig::default(), None);
        let mut fillers = Vec::with_capacity(RATE_GUARD_RECLAIM_BATCH + 1);
        for index in 0..=RATE_GUARD_RECLAIM_BATCH {
            fillers.push(policy.rate_guard(
                CallKind::Chat,
                "anthropic",
                &format!("skipped-filler-{index}"),
            ));
        }

        let credential = "skipped-live-state";
        let cache_key = QuotaIdentity::new("anthropic", credential).guard_cache_key("chat");
        let guard = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        guard
            .record_rate_limited(Some(Duration::from_secs(10)))
            .await;
        guard
            .note_request()
            .await
            .expect("request should be counted");
        for _ in 0..ProviderPacingConfig::default().retry_budget_floor {
            assert!(guard.allow_retry().await, "retry floor should be available");
        }
        assert!(
            !guard.allow_retry().await,
            "retry floor should be exhausted"
        );
        drop(guard);

        {
            let mut cache = policy.guards.lock().unwrap_or_else(PoisonError::into_inner);
            cache.cleanup_cursor = 0;
            cache.cleanup_inspections = 0;
        }
        drop(policy.rate_guard(CallKind::Chat, "anthropic", "skipped-filler-0"));
        assert_eq!(
            guard_cleanup_inspections(&policy),
            0,
            "an existing-key hot lookup must not start a guard cleanup cycle"
        );

        drop(policy.rate_guard(CallKind::Chat, "anthropic", "skipped-trigger-0"));
        assert_eq!(
            guard_cleanup_inspections(&policy),
            RATE_GUARD_RECLAIM_BATCH,
            "one new key must inspect only the fixed guard cleanup batch"
        );
        assert!(
            guard_cache_contains(&policy, &cache_key),
            "the unvisited cache-only guard must retain its live state"
        );
        let retained = policy.rate_guard(CallKind::Chat, "anthropic", credential);
        assert!(
            retained
                .pause_remaining()
                .await
                .expect("retained cooldown should be readable")
                .is_some()
        );
        assert!(
            !retained.allow_retry().await,
            "a skipped cleanup cycle must not reset the exhausted retry budget"
        );
        drop(retained);

        tokio::time::advance(Duration::from_millis(
            ProviderPacingConfig::default().retry_budget_window_ms + 1,
        ))
        .await;
        drop(policy.rate_guard(CallKind::Chat, "anthropic", "skipped-trigger-1"));
        assert!(
            !guard_cache_contains(&policy, &cache_key),
            "the later bounded pass must reclaim state after rotate expires its retry window"
        );
        drop(fillers);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_store_reaches_the_fail_closed_policy_after_the_operation_deadline() {
        // Pins: a connected-but-hung Redis operation cannot park provider
        // admission forever. The fixed operation deadline must surface as a
        // coordination failure that the configured policy can reject.
        let concurrency = ProviderConcurrencyConfig {
            on_coordination_failure: CoordinationFailurePolicy::FailClosed,
            ..ProviderConcurrencyConfig::default()
        };
        let pacing = ProviderPacingConfig {
            scope: ConcurrencyScope::Global,
            ..ProviderPacingConfig::default()
        };
        let coordination =
            ProviderCoordination::new(concurrency, pacing, Some(Arc::new(HangingStore)))
                .expect("coordination should construct with an injected store");
        let guard = coordination.rate_guard(CallKind::Chat, "anthropic", "hung-key");

        let error = guard
            .pause_remaining()
            .await
            .expect_err("fail-closed must reject after the store deadline");
        assert!(
            matches!(error, MoaError::RateLimited { retries: 0, .. }),
            "deadline must reach the typed fail-closed result: {error}"
        );
    }

    #[test]
    fn quota_keys_are_stable_opaque_and_match_each_control_scope() {
        // Pins: every quota control carries only its actual scope, remains
        // deterministic, and never exposes the raw credential.
        let cohere = QuotaIdentity::new("cohere", "secret-key");
        let same = QuotaIdentity::new("cohere", "secret-key");
        let other_key = QuotaIdentity::new("cohere", "other-key");
        let other_provider = QuotaIdentity::new("openai", "secret-key");

        let pacing = cohere.pacing_key("embed-v4.0", "inputs");
        assert_eq!(pacing, same.pacing_key("embed-v4.0", "inputs"));
        assert_ne!(pacing, other_key.pacing_key("embed-v4.0", "inputs"));
        assert_ne!(pacing, other_provider.pacing_key("embed-v4.0", "inputs"));
        assert_ne!(pacing, cohere.pacing_key("embed-v3.0", "inputs"));
        assert_ne!(pacing, cohere.pacing_key("embed-v4.0", "requests"));

        let cooldown = cohere.cooldown_key("embedding");
        let retry_budget = cohere.retry_budget_key("embedding");
        let guard_cache = cohere.guard_cache_key("embedding");
        assert_ne!(cooldown, cohere.cooldown_key("chat"));
        assert_ne!(cooldown, retry_budget);
        assert_ne!(cooldown, guard_cache);

        assert!(
            [&pacing, &cooldown, &retry_budget, &guard_cache]
                .into_iter()
                .all(|key| !key.contains("secret-key")),
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
