//! Cross-replica provider concurrency via TTL leases in the runtime store.
//!
//! Process-local [`ConcurrencyLimiter`](super::concurrency::ConcurrencyLimiter)
//! bounds in-flight calls per process, but an autoscaled fleet then multiplies a
//! shared provider/API-key quota by the replica count. [`GlobalConcurrency`]
//! coordinates one ceiling across replicas by recording a short-lived lease per
//! held slot in a per-key structure in the runtime coordination store
//! ([`RuntimeCacheStore`]). Admission is a compare-and-set over the lease set:
//! stale leases (from a crashed replica) are pruned by TTL as part of every
//! acquisition, so a killed pod's slots self-reclaim.
//!
//! Availability over strict bounding: if the coordination store is unavailable,
//! acquisition degrades open to a process-local semaphore of the same size rather
//! than failing provider calls.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use moa_core::traits::RuntimeCacheStore;
use moa_core::{MoaError, Result};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use super::concurrency::PermitLease;

/// One held global-concurrency slot recorded in the shared lease set.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Lease {
    /// Globally-unique lease identity for this held slot.
    id: String,
    /// Wall-clock expiry (ms since epoch); the crash backstop for the slot.
    expires_at_ms: u64,
}

/// Millisecond wall clock, injectable so lease expiry is deterministic in tests.
#[derive(Clone)]
pub(crate) struct MillisClock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl MillisClock {
    /// The real system clock.
    pub(crate) fn system() -> Self {
        Self(Arc::new(system_now_ms))
    }

    /// A manually-advanced clock backed by a shared counter (tests only).
    #[cfg(test)]
    pub(crate) fn manual(source: Arc<AtomicU64>) -> Self {
        Self(Arc::new(move || source.load(Ordering::SeqCst)))
    }

    fn now_ms(&self) -> u64 {
        (self.0)()
    }
}

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

/// Cross-replica in-flight gate backed by a TTL lease set in the runtime store.
pub(crate) struct GlobalConcurrency {
    store: Arc<dyn RuntimeCacheStore>,
    key: String,
    limit: usize,
    lease_ttl: Duration,
    clock: MillisClock,
    provider: String,
    /// Call-kind label for metrics only; the budget key is per (provider, credential).
    kind: &'static str,
    /// Degrade-open local bound used when the coordination store errors. Shared
    /// per (provider, credential), so a degraded fleet still shares one budget.
    local_fallback: Arc<Semaphore>,
}

impl GlobalConcurrency {
    /// Builds a global limiter for one `(provider, credential)` budget.
    pub(crate) fn new(
        store: Arc<dyn RuntimeCacheStore>,
        key: String,
        limit: usize,
        lease_ttl: Duration,
        provider: String,
        kind: &'static str,
        local_fallback: Arc<Semaphore>,
    ) -> Self {
        Self {
            store,
            key,
            limit,
            lease_ttl,
            clock: MillisClock::system(),
            provider,
            kind,
            local_fallback,
        }
    }

    /// Test constructor with an injectable clock and store.
    #[cfg(test)]
    pub(crate) fn for_test(
        store: Arc<dyn RuntimeCacheStore>,
        key: impl Into<String>,
        limit: usize,
        lease_ttl: Duration,
        clock: MillisClock,
    ) -> Self {
        Self {
            store,
            key: key.into(),
            limit,
            lease_ttl,
            clock,
            provider: "test".to_string(),
            kind: "test",
            local_fallback: Arc::new(Semaphore::new(limit)),
        }
    }

    /// Acquires a global slot, waiting at most `max_wait`.
    ///
    /// Returns `None` when the shared gate stays saturated for the whole wait
    /// (the same failover-eligible signal as the local limiter). A coordination-
    /// store failure degrades to the local fallback rather than blocking calls.
    pub(crate) async fn acquire(&self, max_wait: Duration) -> Option<PermitLease> {
        let deadline = Instant::now() + max_wait;
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
                Err(error) => {
                    tracing::warn!(
                        provider = %self.provider,
                        error = %error,
                        "global concurrency store unavailable; degrading to local limiter"
                    );
                    metrics::counter!(
                        "moa_provider_concurrency_degraded_local_total",
                        "provider" => self.provider.clone(),
                        "kind" => self.kind,
                    )
                    .increment(1);
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

    /// One admission attempt: prune expired leases, then CAS-add ours if there is
    /// room. `Ok(None)` means the gate was full or another writer won the CAS
    /// race; `Err` means the store failed and the caller should degrade.
    async fn try_acquire_once(&self) -> Result<Option<GlobalLeaseGuard>> {
        let raw = self.store.get(&self.key).await?;
        let mut leases = decode_leases(raw.as_deref());
        let now = self.clock.now_ms();
        leases.retain(|lease| lease.expires_at_ms > now);
        if leases.len() >= self.limit {
            return Ok(None);
        }

        let id = next_lease_id();
        leases.push(Lease {
            id: id.clone(),
            expires_at_ms: now + self.lease_ttl.as_millis() as u64,
        });
        let live = leases.len();
        let encoded = encode_leases(&leases)?;
        if self
            .store
            .compare_and_set(&self.key, raw.as_deref(), encoded, self.lease_ttl)
            .await?
        {
            metrics::gauge!(
                "moa_provider_concurrency_lease_count",
                "provider" => self.provider.clone(),
                "kind" => self.kind,
            )
            .set(live as f64);
            Ok(Some(GlobalLeaseGuard {
                store: Arc::clone(&self.store),
                key: self.key.clone(),
                id,
                lease_ttl: self.lease_ttl,
                clock: self.clock.clone(),
            }))
        } else {
            Ok(None)
        }
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
/// loses every CAS race, the lease's TTL reclaims the slot as the backstop.
pub(crate) struct GlobalLeaseGuard {
    store: Arc<dyn RuntimeCacheStore>,
    key: String,
    id: String,
    lease_ttl: Duration,
    clock: MillisClock,
}

impl Drop for GlobalLeaseGuard {
    fn drop(&mut self) {
        let store = Arc::clone(&self.store);
        let key = std::mem::take(&mut self.key);
        let id = std::mem::take(&mut self.id);
        let lease_ttl = self.lease_ttl;
        let clock = self.clock.clone();
        if let Ok(handle) = Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) =
                    release_lease(store.as_ref(), &key, &id, lease_ttl, &clock).await
                {
                    tracing::debug!(
                        error = %error,
                        "global concurrency lease release failed; TTL will reclaim the slot"
                    );
                }
            });
        }
    }
}

/// Removes one lease id from the shared set, pruning expired entries too.
async fn release_lease(
    store: &dyn RuntimeCacheStore,
    key: &str,
    id: &str,
    ttl: Duration,
    clock: &MillisClock,
) -> Result<()> {
    for _ in 0..5 {
        let Some(raw) = store.get(key).await? else {
            return Ok(());
        };
        let mut leases = decode_leases(Some(&raw));
        let now = clock.now_ms();
        leases.retain(|lease| lease.id != id && lease.expires_at_ms > now);
        let encoded = encode_leases(&leases)?;
        if store.compare_and_set(key, Some(&raw), encoded, ttl).await? {
            return Ok(());
        }
    }
    // The lease's own TTL is the backstop when release keeps losing CAS races.
    Ok(())
}

fn decode_leases(raw: Option<&[u8]>) -> Vec<Lease> {
    raw.and_then(|bytes| serde_json::from_slice(bytes).ok())
        .unwrap_or_default()
}

fn encode_leases(leases: &[Lease]) -> Result<Vec<u8>> {
    serde_json::to_vec(leases).map_err(|error| {
        MoaError::SerializationError(format!("encode vector sync leases: {error}"))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;

    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;

    /// A minimal in-memory store with a manually-advanced clock so lease TTLs are
    /// deterministic. Values never expire on their own — expiry is modeled purely
    /// through the lease `expires_at_ms` and the injected [`MillisClock`].
    #[derive(Default)]
    struct TestStore {
        entries: Mutex<HashMap<String, Vec<u8>>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl TestStore {
        fn shared() -> Arc<Self> {
            Arc::new(Self::default())
        }
        fn set_failing(&self, failing: bool) {
            self.fail.store(failing, Ordering::SeqCst);
        }
        async fn lease_count(&self, key: &str) -> usize {
            let entries = self.entries.lock().await;
            entries
                .get(key)
                .map(|raw| decode_leases(Some(raw)).len())
                .unwrap_or(0)
        }
    }

    #[async_trait]
    impl RuntimeCacheStore for TestStore {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(MoaError::StorageError("test store unavailable".to_string()));
            }
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
            if self.fail.load(Ordering::SeqCst) {
                return Err(MoaError::StorageError("test store unavailable".to_string()));
            }
            let mut entries = self.entries.lock().await;
            let current = entries.get(key).map(|value| value.as_slice());
            if current != expected {
                return Ok(false);
            }
            entries.insert(key.to_string(), value);
            Ok(true)
        }
        async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
            Ok(())
        }
    }

    fn limiter(store: Arc<TestStore>, clock: MillisClock, limit: usize) -> GlobalConcurrency {
        GlobalConcurrency::for_test(
            store,
            "moa:concurrency:test",
            limit,
            Duration::from_secs(60),
            clock,
        )
    }

    #[tokio::test]
    async fn two_handles_sharing_a_store_enforce_a_combined_limit() {
        // Pins: leases in one shared store bound total in-flight across independent
        // limiter handles (the multi-replica case) to the configured limit.
        let store = TestStore::shared();
        let clock = MillisClock::manual(Arc::new(AtomicU64::new(1_000)));
        let replica_a = limiter(Arc::clone(&store), clock.clone(), 2);
        let replica_b = limiter(Arc::clone(&store), clock.clone(), 2);

        let a1 = replica_a
            .acquire(Duration::from_millis(50))
            .await
            .expect("slot 1");
        let b1 = replica_b
            .acquire(Duration::from_millis(50))
            .await
            .expect("slot 2");
        assert_eq!(store.lease_count("moa:concurrency:test").await, 2);

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

    #[tokio::test]
    async fn expired_lease_frees_a_slot_after_a_simulated_crash() {
        // Pins: a crashed replica that never releases its lease does not strand the
        // slot — the TTL prunes it and a later caller acquires.
        let store = TestStore::shared();
        let now = Arc::new(AtomicU64::new(1_000));
        let clock = MillisClock::manual(Arc::clone(&now));
        let limiter = limiter(Arc::clone(&store), clock, 1);

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
        now.fetch_add(61_000, Ordering::SeqCst);
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_some(),
            "the expired lease must free the slot"
        );
    }

    #[tokio::test]
    async fn saturated_gate_returns_none_at_the_deadline() {
        // Pins: a full shared gate returns the failover-eligible None once the wait
        // elapses rather than blocking indefinitely.
        let store = TestStore::shared();
        let clock = MillisClock::manual(Arc::new(AtomicU64::new(1_000)));
        let limiter = limiter(Arc::clone(&store), clock, 1);
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
    async fn store_failure_degrades_to_the_local_bound() {
        // Pins: when the coordination store errors, acquisition degrades open to a
        // local semaphore of the same size instead of failing the call — and still
        // bounds concurrency locally.
        let store = TestStore::shared();
        let clock = MillisClock::manual(Arc::new(AtomicU64::new(1_000)));
        let limiter = limiter(Arc::clone(&store), clock, 1);
        store.set_failing(true);

        let first = limiter
            .acquire(Duration::from_millis(50))
            .await
            .expect("degrades to a local slot rather than failing");
        // The local fallback still enforces the bound of 1.
        assert!(
            limiter.acquire(Duration::from_millis(50)).await.is_none(),
            "the degraded local limiter still bounds in-flight calls"
        );
        drop(first);
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
            MillisClock::system(),
        );
        let replica_b = GlobalConcurrency::for_test(
            Arc::clone(&store),
            key.clone(),
            2,
            ttl,
            MillisClock::system(),
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
