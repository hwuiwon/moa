//! Cross-replica provider concurrency via TTL leases in the runtime store.
//!
//! Process-local [`ConcurrencyLimiter`](super::concurrency::ConcurrencyLimiter)
//! bounds in-flight calls per process, but an autoscaled fleet then multiplies a
//! shared provider/API-key quota by the replica count. [`GlobalConcurrency`]
//! coordinates one ceiling across replicas through the runtime store's bounded
//! atomic lease operation
//! ([`try_acquire_bounded_lease`](RuntimeCacheStore::try_acquire_bounded_lease)):
//! admission, expiry pruning, and the live count all happen inside one atomic
//! store operation, so two replicas racing for the last slot cannot both win.
//! A killed pod's slots self-reclaim when their TTL expires.
//!
//! When the coordination store fails, the configured
//! [`CoordinationFailurePolicy`] decides: `bounded_degraded` falls back to this
//! replica's local semaphore and says so loudly (metric, warning, duration),
//! while `fail_closed` rejects admission rather than enforcing a ceiling that is
//! no longer fleet-wide.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moa_config::CoordinationFailurePolicy;
use moa_core::error::Result;
use moa_core::traits::RuntimeCacheStore;
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
// The admission deadline must share the clock the coordination store and the
// sleeps below use, so a paused-time test cannot advance one without the other.
use tokio::time::{Instant, sleep};

use super::concurrency::PermitLease;
use super::concurrency_factory::{
    CoordinatedControl, record_coordination_degraded, record_coordination_rejected,
};

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Returns a lease id unique across replicas and within this process.
///
/// The process id plus a high-resolution timestamp separate replicas; a
/// monotonic counter separates leases within one process. No RNG dependency.
fn next_lease_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", std::process::id(), system_now_ms(), sequence)
}

/// Cross-replica in-flight gate backed by a bounded TTL lease set in the store.
pub(crate) struct GlobalConcurrency {
    store: Arc<dyn RuntimeCacheStore>,
    key: String,
    limit: usize,
    lease_ttl: Duration,
    provider: String,
    /// Call-kind label for metrics only; the budget key is per (provider, credential).
    kind: &'static str,
    /// Degrade-open local bound used when the coordination store errors. Shared
    /// per (provider, credential), so a degraded fleet still shares one budget.
    local_fallback: Arc<Semaphore>,
    /// What to do when the coordination store cannot answer.
    on_failure: CoordinationFailurePolicy,
}

impl GlobalConcurrency {
    /// Builds a global limiter for one `(provider, credential)` budget.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<dyn RuntimeCacheStore>,
        key: String,
        limit: usize,
        lease_ttl: Duration,
        provider: String,
        kind: &'static str,
        local_fallback: Arc<Semaphore>,
        on_failure: CoordinationFailurePolicy,
    ) -> Self {
        Self {
            store,
            key,
            limit,
            lease_ttl,
            provider,
            kind,
            local_fallback,
            on_failure,
        }
    }

    /// Test constructor with an explicit store and failure policy.
    #[cfg(test)]
    pub(crate) fn for_test(
        store: Arc<dyn RuntimeCacheStore>,
        key: impl Into<String>,
        limit: usize,
        lease_ttl: Duration,
        on_failure: CoordinationFailurePolicy,
    ) -> Self {
        Self {
            store,
            key: key.into(),
            limit,
            lease_ttl,
            provider: "test".to_string(),
            kind: "test",
            local_fallback: Arc::new(Semaphore::new(limit)),
            on_failure,
        }
    }

    /// Acquires a global slot, waiting at most `max_wait`.
    ///
    /// Returns `None` when the shared gate stays saturated for the whole wait
    /// (the same failover-eligible signal as the local limiter), and also when a
    /// coordination failure is met with `fail_closed`. A coordination failure
    /// under `bounded_degraded` falls back to the local semaphore instead.
    pub(crate) async fn acquire(&self, max_wait: Duration) -> Option<PermitLease> {
        let started = Instant::now();
        let deadline = started + max_wait;
        let mut backoff = Duration::from_millis(10);
        loop {
            match self.try_acquire_once().await {
                Ok(Some(guard)) => {
                    metrics::counter!(
                        "moa_provider_concurrency_global_acquired_total",
                        "provider" => self.provider.clone(),
                        "kind" => self.kind,
                    )
                    .increment(1);
                    return Some(PermitLease::Global(guard));
                }
                Ok(None) => {}
                // A store failure is never retried here: retrying a broken store
                // in the admission path is exactly the storm this control exists
                // to prevent. One failure decides the outcome by policy.
                Err(error) => {
                    if self.on_failure.rejects_admission() {
                        record_coordination_rejected(
                            &self.provider,
                            CoordinatedControl::Concurrency,
                            &error,
                        );
                        return None;
                    }
                    record_coordination_degraded(
                        &self.provider,
                        CoordinatedControl::Concurrency,
                        started.elapsed(),
                        &error,
                    );
                    return self.acquire_local_fallback(deadline).await;
                }
            }

            let now = Instant::now();
            if now >= deadline {
                metrics::counter!(
                    "moa_provider_concurrency_saturated_total",
                    "provider" => self.provider.clone(),
                    "kind" => self.kind,
                )
                .increment(1);
                return None;
            }
            let remaining = deadline.saturating_duration_since(now);
            sleep(backoff.min(remaining)).await;
            backoff = (backoff * 2).min(Duration::from_millis(100));
        }
    }

    /// One admission attempt against the store's bounded lease set.
    ///
    /// `Ok(None)` means the gate was full; `Err` means the store failed and the
    /// caller applies the coordination-failure policy.
    async fn try_acquire_once(&self) -> Result<Option<GlobalLeaseGuard>> {
        let lease_id = next_lease_id();
        let decision = self
            .store
            .try_acquire_bounded_lease(&self.key, &lease_id, self.limit, self.lease_ttl)
            .await?;
        if !decision.acquired {
            return Ok(None);
        }
        metrics::gauge!(
            "moa_provider_concurrency_lease_count",
            "provider" => self.provider.clone(),
            "kind" => self.kind,
        )
        .set(decision.live as f64);
        Ok(Some(GlobalLeaseGuard {
            store: Arc::clone(&self.store),
            key: self.key.clone(),
            id: lease_id,
        }))
    }

    async fn acquire_local_fallback(&self, deadline: Instant) -> Option<PermitLease> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, Arc::clone(&self.local_fallback).acquire_owned())
            .await
        {
            Ok(Ok(permit)) => Some(PermitLease::Held(permit)),
            // The semaphore is never closed while this limiter holds the Arc.
            Ok(Err(_)) => Some(PermitLease::Unbounded),
            Err(_) => {
                metrics::counter!(
                    "moa_provider_concurrency_saturated_total",
                    "provider" => self.provider.clone(),
                    "kind" => self.kind,
                )
                .increment(1);
                None
            }
        }
    }
}

/// An acquired global slot; releasing it deletes the lease from the shared set.
///
/// `Drop` spawns a best-effort async release. If that cannot run (no runtime) or
/// fails, the lease's TTL reclaims the slot as the backstop.
pub(crate) struct GlobalLeaseGuard {
    store: Arc<dyn RuntimeCacheStore>,
    key: String,
    id: String,
}

impl Drop for GlobalLeaseGuard {
    fn drop(&mut self) {
        let store = Arc::clone(&self.store);
        let key = std::mem::take(&mut self.key);
        let id = std::mem::take(&mut self.id);
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = store.release_bounded_lease(&key, &id).await {
                    tracing::debug!(
                        error = %error,
                        "global concurrency lease release failed; TTL will reclaim the slot"
                    );
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::error::MoaError;
    use moa_core::traits::BoundedLeaseDecision;
    use moa_runtime_store::MemoryRuntimeCacheStore;

    use super::*;

    /// A store that fails every coordination operation, for the policy tests.
    struct FailingStore;

    #[async_trait::async_trait]
    impl RuntimeCacheStore for FailingStore {
        async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn delete(&self, _key: &str) -> Result<()> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn compare_and_set(
            &self,
            _key: &str,
            _expected: Option<&[u8]>,
            _value: Vec<u8>,
            _ttl: Duration,
        ) -> Result<bool> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn try_acquire_bounded_lease(
            &self,
            _key: &str,
            _lease_id: &str,
            _limit: usize,
            _ttl: Duration,
        ) -> Result<BoundedLeaseDecision> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
        async fn release_bounded_lease(&self, _key: &str, _lease_id: &str) -> Result<usize> {
            Err(MoaError::StorageError("test store unavailable".to_string()))
        }
    }

    fn limiter(
        store: Arc<dyn RuntimeCacheStore>,
        limit: usize,
        ttl: Duration,
    ) -> GlobalConcurrency {
        GlobalConcurrency::for_test(
            store,
            "moa:concurrency:test",
            limit,
            ttl,
            CoordinationFailurePolicy::BoundedDegraded,
        )
    }

    #[tokio::test]
    async fn two_handles_sharing_a_store_enforce_a_combined_limit() {
        // Pins: leases in one shared store bound total in-flight across independent
        // limiter handles (the multi-replica case) to the configured limit.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let replica_a = limiter(Arc::clone(&store), 2, Duration::from_secs(60));
        let replica_b = limiter(Arc::clone(&store), 2, Duration::from_secs(60));

        let a1 = replica_a
            .acquire(Duration::from_millis(50))
            .await
            .expect("slot 1");
        let b1 = replica_b
            .acquire(Duration::from_millis(50))
            .await
            .expect("slot 2");

        // The combined limit of 2 is reached; a third caller on either handle is
        // saturated.
        assert!(
            replica_a.acquire(Duration::from_millis(50)).await.is_none(),
            "the shared gate must reject the third holder"
        );
        assert!(replica_b.acquire(Duration::from_millis(50)).await.is_none());

        drop(a1);
        drop(b1);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_lease_frees_a_slot_after_a_simulated_crash() {
        // Pins: a crashed replica that never releases its lease does not strand the
        // slot — the store's TTL prunes it and a later caller acquires.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let limiter = limiter(Arc::clone(&store), 1, Duration::from_secs(60));

        // Simulate a crashed holder: a lease that is never released via Drop.
        std::mem::forget(
            limiter
                .acquire(Duration::from_millis(50))
                .await
                .expect("first slot"),
        );
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_none(),
            "the single slot is held by the crashed lease"
        );

        // Advance past the 60s lease TTL: the stale lease is pruned on acquire.
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_some(),
            "the expired lease must free the slot"
        );
    }

    #[tokio::test]
    async fn saturated_gate_returns_none_at_the_deadline() {
        // Pins: a full shared gate returns the failover-eligible None once the wait
        // elapses rather than blocking indefinitely.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let limiter = limiter(store, 1, Duration::from_secs(60));
        std::mem::forget(
            limiter
                .acquire(Duration::from_millis(10))
                .await
                .expect("slot"),
        );

        let started = Instant::now();
        assert!(limiter.acquire(Duration::from_millis(30)).await.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "acquisition must give up near the deadline, not hang"
        );
    }

    #[tokio::test]
    async fn store_failure_degrades_to_the_local_bound_under_the_default_policy() {
        // Pins: with bounded_degraded, a coordination-store failure falls back to a
        // local semaphore of the same size instead of failing the call — and that
        // fallback still bounds in-flight calls within this replica.
        let limiter = limiter(Arc::new(FailingStore), 1, Duration::from_secs(60));

        let first = limiter
            .acquire(Duration::from_millis(50))
            .await
            .expect("degrades to a local slot rather than failing");
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_none(),
            "the degraded local limiter still bounds in-flight calls"
        );
        drop(first);
    }

    #[tokio::test]
    async fn store_failure_rejects_admission_under_fail_closed() {
        // Pins: fail_closed refuses the very first admission when coordination is
        // unavailable, instead of enforcing a per-replica ceiling that the fleet
        // would multiply. This is the case bounded_degraded deliberately allows,
        // so the two policies cannot be confused.
        let limiter = GlobalConcurrency::for_test(
            Arc::new(FailingStore),
            "moa:concurrency:fail-closed",
            4,
            Duration::from_secs(60),
            CoordinationFailurePolicy::FailClosed,
        );

        let started = Instant::now();
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_none(),
            "fail_closed must reject admission when the store is unavailable"
        );
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "rejection must be immediate, not a retry loop against a broken store"
        );
    }

    /// Live coverage against a real Redis/Valkey coordination store. Requires
    /// `MOA_RUN_LIVE_REDIS=1` and a reachable Redis at `MOA_RUN_LIVE_REDIS_URL`
    /// (the local compose stack exposes Valkey; default `redis://127.0.0.1:6379`).
    #[tokio::test]
    #[ignore = "requires a live Redis; set MOA_RUN_LIVE_REDIS=1"]
    async fn global_limiter_enforces_a_shared_limit_over_live_redis() {
        let enabled = std::env::var("MOA_RUN_LIVE_REDIS")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if !enabled {
            panic!("MOA_RUN_LIVE_REDIS=1 is required to run the live Redis concurrency test");
        }
        let url = std::env::var("MOA_RUN_LIVE_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(
            moa_runtime_store::RedisRuntimeCacheStore::new(&url)
                .await
                .expect("connect to live Redis"),
        );
        let key = format!("moa:test:concurrency:{}", system_now_ms());
        let ttl = Duration::from_secs(30);
        let replica_a = GlobalConcurrency::for_test(
            Arc::clone(&store),
            key.clone(),
            2,
            ttl,
            CoordinationFailurePolicy::BoundedDegraded,
        );
        let replica_b = GlobalConcurrency::for_test(
            Arc::clone(&store),
            key.clone(),
            2,
            ttl,
            CoordinationFailurePolicy::BoundedDegraded,
        );

        let slot_a = replica_a
            .acquire(Duration::from_millis(200))
            .await
            .expect("first shared slot");
        let slot_b = replica_b
            .acquire(Duration::from_millis(200))
            .await
            .expect("second shared slot");
        assert!(
            replica_a
                .acquire(Duration::from_millis(200))
                .await
                .is_none(),
            "the shared Redis gate must reject the third holder"
        );

        // Releasing a slot deletes its lease from Redis (RAII drop spawns release).
        drop(slot_a);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let slot_c = replica_b
            .acquire(Duration::from_millis(500))
            .await
            .expect("a released slot frees for the next caller");

        drop(slot_b);
        drop(slot_c);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = store.delete(&key).await;
    }
}
