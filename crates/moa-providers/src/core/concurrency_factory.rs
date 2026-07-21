//! Builds per-provider concurrency limiters from provider configuration.
//!
//! Provider rate limits are tied to the account tier, so the in-flight budget is
//! **per (provider, credential)** — one shared ceiling that every call kind the
//! credential serves (e.g. Cohere embed + rerank on one key) draws from. The
//! effective limit is the provider's own `max_concurrent_requests`, else the
//! workspace-wide
//! [`default_max_in_flight`](moa_config::ProviderConcurrencyConfig).
//!
//! Local scope shares one process-local semaphore per budget key (so the two
//! Cohere clients above contend for one budget); global scope shares one lease
//! key in the runtime coordination store across replicas. That store is a single
//! per-process handle installed once by the composition layer via
//! [`install_coordination_store`]; limiter construction runs in `from_config`,
//! where the config is already in scope.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use moa_config::MoaConfig;
use moa_config::{ConcurrencyScope, ProviderConcurrencyConfig};
use moa_core::traits::RuntimeCacheStore;
use tokio::sync::Semaphore;

use super::concurrency::ConcurrencyLimiter;
use super::global_concurrency::GlobalConcurrency;

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
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
        }
    }
}

/// Process-wide runtime coordination store used to build global limiters.
///
/// Installed once by the composition layer (runtime deps). When absent — single
/// node, dev, or tests — `global` scope falls back to a process-local limiter.
static COORDINATION_STORE: OnceLock<Arc<dyn RuntimeCacheStore>> = OnceLock::new();

/// Process-local budgets: one shared semaphore per `(provider, credential)` so
/// every call kind on a credential contends for the same in-flight budget.
static LOCAL_BUDGETS: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();

/// Installs the runtime coordination store for global concurrency (idempotent).
pub fn install_coordination_store(store: Arc<dyn RuntimeCacheStore>) {
    let _ = COORDINATION_STORE.set(store);
}

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

/// Concurrency policy resolved from config plus the optional coordination store.
#[derive(Clone)]
pub(crate) struct ProviderConcurrency {
    config: ProviderConcurrencyConfig,
    store: Option<Arc<dyn RuntimeCacheStore>>,
}

impl ProviderConcurrency {
    /// Resolves the concurrency policy from config and the installed store.
    pub(crate) fn from_config(config: &MoaConfig) -> Self {
        Self {
            config: config.providers.concurrency.clone(),
            store: COORDINATION_STORE.get().cloned(),
        }
    }

    /// Test constructor with an explicit (optional) store.
    #[cfg(test)]
    pub(crate) fn with_store(
        config: ProviderConcurrencyConfig,
        store: Option<Arc<dyn RuntimeCacheStore>>,
    ) -> Self {
        Self { config, store }
    }

    /// Builds the shared limiter for one `(provider, credential)` budget.
    ///
    /// The effective limit is the provider's `max_concurrent_requests` when set,
    /// else the workspace `default_max_in_flight` (`0` = unbounded). Global scope
    /// with a positive limit and an installed store yields a cross-replica limiter
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
        let limit = per_provider_override.unwrap_or(self.config.default_max_in_flight) as usize;
        let block_threshold = Duration::from_millis(self.config.block_threshold_ms);
        let key = budget_key(provider, credential);

        match (self.config.scope, &self.store) {
            (ConcurrencyScope::Global, Some(store)) if limit > 0 => {
                // The degrade-open fallback is the same shared per-(provider,
                // credential) semaphore as the local path, so kinds still share
                // one budget when the coordination store is unavailable.
                let fallback = local_budget_semaphore(&key, limit);
                let global = GlobalConcurrency::new(
                    Arc::clone(store),
                    key,
                    limit,
                    Duration::from_millis(self.config.lease_ttl_ms),
                    provider.to_string(),
                    kind.label(),
                    fallback,
                );
                ConcurrencyLimiter::global(global, block_threshold)
            }
            _ => {
                let semaphore = (limit > 0).then(|| local_budget_semaphore(&key, limit));
                ConcurrencyLimiter::from_local_semaphore(semaphore, block_threshold)
            }
        }
    }
}

/// Builds the shared budget key for one `(provider, credential)`, hashing the
/// credential so the raw API key never lands in the coordination store.
fn budget_key(provider: &str, credential: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    credential.hash(&mut hasher);
    format!(
        "moa:provider-concurrency:{provider}:{:016x}",
        hasher.finish()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use moa_core::error::Result;
    use tokio::sync::Mutex;

    use super::*;

    fn global_config() -> ProviderConcurrencyConfig {
        ProviderConcurrencyConfig {
            scope: ConcurrencyScope::Global,
            ..ProviderConcurrencyConfig::default()
        }
    }

    /// Minimal in-memory coordination store for the global-scope factory test.
    #[derive(Default)]
    struct TestStore {
        entries: Mutex<HashMap<String, Vec<u8>>>,
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
    }

    #[test]
    fn local_scope_builds_a_local_bounded_limiter() {
        // Pins: the default (local) scope yields a bounded process-local limiter
        // sized by the workspace default, ignoring any installed store.
        let policy = ProviderConcurrency::with_store(ProviderConcurrencyConfig::default(), None);
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
        let policy = ProviderConcurrency::with_store(config, None);
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
    fn global_scope_without_a_store_falls_back_to_local() {
        // Pins: global scope with no installed coordination store degrades to a
        // local limiter so single-node/dev deployments still work.
        let policy = ProviderConcurrency::with_store(global_config(), None);
        let limiter = policy.limiter(
            CallKind::Embedding,
            "openai",
            "global-no-store-key",
            Some(4),
        );
        assert!(limiter.is_bounded());
    }

    #[test]
    fn per_provider_override_of_zero_is_unbounded() {
        // Pins: an explicit 0 override opts back into unbounded, even under global
        // scope (no coordination for an unbounded budget).
        let policy = ProviderConcurrency::with_store(global_config(), None);
        let limiter = policy.limiter(CallKind::Chat, "anthropic", "unbounded-key", Some(0));
        assert!(!limiter.is_bounded());
    }

    #[tokio::test]
    async fn call_kinds_on_one_credential_share_a_local_budget() {
        // Pins: limiters built for the same (provider, credential) — the embed,
        // rerank, and chat clients on one Cohere key — contend for one shared
        // process-local budget; a different provider has an independent budget.
        let policy = ProviderConcurrency::with_store(ProviderConcurrencyConfig::default(), None);
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
        let policy = ProviderConcurrency::with_store(global_config(), Some(store));
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
    fn budget_key_is_stable_per_provider_credential_and_hides_the_credential() {
        // Pins: the budget key is deterministic per (provider, credential), differs
        // across providers and credentials, and never contains the raw key.
        let a = budget_key("cohere", "secret-key");
        let b = budget_key("cohere", "secret-key");
        let c = budget_key("cohere", "other-key");
        let d = budget_key("openai", "secret-key");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(!a.contains("secret-key"));
        assert!(a.starts_with("moa:provider-concurrency:cohere:"));
    }
}
