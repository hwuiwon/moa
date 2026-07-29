//! Runtime cache store implementations.

mod memory;

#[cfg(feature = "redis")]
mod redis;

pub use memory::MemoryRuntimeCacheStore;

#[cfg(feature = "redis")]
pub use redis::RedisRuntimeCacheStore;

use std::sync::Arc;
use std::time::Duration;

use moa_config::{RuntimeCacheBackend, RuntimeCacheConfig};
use moa_core::error::{MoaError, Result};
use moa_core::traits::{
    BoundedLeaseDecision, RateTokenDecision, RetryBudgetDecision, RuntimeCacheStore,
};

/// Runtime-cache decorator that bounds every backend operation.
///
/// Provider coordination wraps its cache with this at the composition boundary
/// so a hung Redis connection becomes the same ordinary store error as a failed
/// connection. The coordination-failure policy can then reject or degrade the
/// request instead of leaving provider admission parked forever.
pub struct DeadlineRuntimeCacheStore {
    inner: Arc<dyn RuntimeCacheStore>,
    deadline: Duration,
}

impl DeadlineRuntimeCacheStore {
    /// Wraps `inner` with one positive operation deadline.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when `deadline` is zero.
    pub fn new(inner: Arc<dyn RuntimeCacheStore>, deadline: Duration) -> Result<Self> {
        if deadline.is_zero() {
            return Err(MoaError::ConfigError(
                "runtime cache operation deadline must be greater than zero".to_string(),
            ));
        }
        Ok(Self { inner, deadline })
    }

    async fn bounded<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::time::timeout(self.deadline, future)
            .await
            .map_err(|_| {
                MoaError::StorageError(format!(
                    "runtime cache {operation} exceeded its {}ms operation deadline",
                    self.deadline.as_millis()
                ))
            })?
    }
}

#[async_trait::async_trait]
impl RuntimeCacheStore for DeadlineRuntimeCacheStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.bounded("get", self.inner.get(key)).await
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        self.bounded("set", self.inner.set(key, value, ttl)).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.bounded("delete", self.inner.delete(key)).await
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool> {
        self.bounded(
            "compare_and_set",
            self.inner.compare_and_set(key, expected, value, ttl),
        )
        .await
    }

    async fn expire(&self, key: &str, ttl: Duration) -> Result<()> {
        self.bounded("expire", self.inner.expire(key, ttl)).await
    }

    async fn try_acquire_bounded_lease(
        &self,
        key: &str,
        lease_id: &str,
        limit: usize,
        ttl: Duration,
    ) -> Result<BoundedLeaseDecision> {
        self.bounded(
            "try_acquire_bounded_lease",
            self.inner
                .try_acquire_bounded_lease(key, lease_id, limit, ttl),
        )
        .await
    }

    async fn release_bounded_lease(&self, key: &str, lease_id: &str) -> Result<usize> {
        self.bounded(
            "release_bounded_lease",
            self.inner.release_bounded_lease(key, lease_id),
        )
        .await
    }

    async fn try_consume_rate_tokens(
        &self,
        key: &str,
        limit_per_min: u32,
        permits: u32,
        ttl: Duration,
    ) -> Result<RateTokenDecision> {
        self.bounded(
            "try_consume_rate_tokens",
            self.inner
                .try_consume_rate_tokens(key, limit_per_min, permits, ttl),
        )
        .await
    }

    async fn extend_cooldown(&self, key: &str, cooldown: Duration) -> Result<Duration> {
        self.bounded("extend_cooldown", self.inner.extend_cooldown(key, cooldown))
            .await
    }

    async fn cooldown_remaining(&self, key: &str) -> Result<Duration> {
        self.bounded("cooldown_remaining", self.inner.cooldown_remaining(key))
            .await
    }

    async fn note_windowed_request(&self, key: &str, window: Duration) -> Result<u64> {
        self.bounded(
            "note_windowed_request",
            self.inner.note_windowed_request(key, window),
        )
        .await
    }

    async fn try_consume_retry_budget(
        &self,
        key: &str,
        window: Duration,
        budget_percent: u32,
        budget_floor: u64,
    ) -> Result<RetryBudgetDecision> {
        self.bounded(
            "try_consume_retry_budget",
            self.inner
                .try_consume_retry_budget(key, window, budget_percent, budget_floor),
        )
        .await
    }
}

/// Environment flag that explicitly allows the process-local memory backend for `auto`.
pub(crate) const MEMORY_BACKEND_OPT_IN_ENV: &str = "MOA_RUNTIME_CACHE_ALLOW_MEMORY";

/// Runtime cache backend after `auto` selection has been resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedRuntimeCacheBackend {
    /// Use a process-local in-memory cache.
    Memory,
    /// Use Redis for shared runtime coordination.
    Redis,
}

/// Resolves the configured runtime cache backend, failing closed for distributed safety.
///
/// The process-local [`ResolvedRuntimeCacheBackend::Memory`] backend is only selected when the
/// operator explicitly opts in — either `runtime_cache.backend = "memory"` or the
/// [`MEMORY_BACKEND_OPT_IN_ENV`] flag. An `auto` backend with no Redis URL and no opt-in returns a
/// clear error instead of silently falling back to a process-local cache, which would make
/// cross-instance coordination wrong in a fleet.
pub fn select_runtime_cache_backend(
    config: &RuntimeCacheConfig,
) -> Result<ResolvedRuntimeCacheBackend> {
    resolve_runtime_cache_backend(
        config.backend,
        has_redis_url(config),
        memory_backend_opt_in(),
    )
}

/// Pure backend resolution, split out so the fail-closed matrix is testable without env or config.
fn resolve_runtime_cache_backend(
    backend: RuntimeCacheBackend,
    has_redis_url: bool,
    memory_opt_in: bool,
) -> Result<ResolvedRuntimeCacheBackend> {
    match backend {
        RuntimeCacheBackend::Redis => Ok(ResolvedRuntimeCacheBackend::Redis),
        // Explicit operator opt-in to the process-local backend.
        RuntimeCacheBackend::Memory => Ok(ResolvedRuntimeCacheBackend::Memory),
        RuntimeCacheBackend::Auto if has_redis_url => Ok(ResolvedRuntimeCacheBackend::Redis),
        RuntimeCacheBackend::Auto if memory_opt_in => Ok(ResolvedRuntimeCacheBackend::Memory),
        RuntimeCacheBackend::Auto => Err(MoaError::ConfigError(format!(
            "runtime_cache.backend is 'auto' but no Redis URL is configured; set \
             runtime_cache.redis_url for distributed coordination, or explicitly opt into the \
             process-local cache with runtime_cache.backend = \"memory\" or {MEMORY_BACKEND_OPT_IN_ENV}=1"
        ))),
    }
}

fn memory_backend_opt_in() -> bool {
    std::env::var(MEMORY_BACKEND_OPT_IN_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
}

fn has_redis_url(config: &RuntimeCacheConfig) -> bool {
    config
        .redis_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_config::{RuntimeCacheBackend, RuntimeCacheConfig};
    use moa_core::error::Result;
    use moa_core::traits::RuntimeCacheStore;
    use tokio::time::advance;

    use super::{
        MemoryRuntimeCacheStore, ResolvedRuntimeCacheBackend, resolve_runtime_cache_backend,
        select_runtime_cache_backend,
    };

    #[tokio::test(start_paused = true)]
    async fn memory_store_sets_gets_expires_and_deletes_values() -> Result<()> {
        // Pins: memory runtime cache values obey set, expire, TTL cleanup, and delete.
        let store = MemoryRuntimeCacheStore::default();

        store
            .set("session:one", b"first".to_vec(), Duration::from_secs(10))
            .await?;
        assert_eq!(store.get("session:one").await?, Some(b"first".to_vec()));

        store.expire("session:one", Duration::from_secs(2)).await?;
        advance(Duration::from_secs(3)).await;
        assert_eq!(store.get("session:one").await?, None);

        store
            .set("session:one", b"second".to_vec(), Duration::from_secs(10))
            .await?;
        store.delete("session:one").await?;
        assert_eq!(store.get("session:one").await?, None);

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn memory_store_sweeps_expired_entries_never_read_again() -> Result<()> {
        // Pins: entries that expire and are never read again are reclaimed by the periodic sweep
        // on the write path, so the memory backend does not grow unboundedly.
        let store = MemoryRuntimeCacheStore::default();

        store
            .set("ghost", b"x".to_vec(), Duration::from_secs(1))
            .await?;
        assert_eq!(store.entry_count().await, 1);

        // Let the entry expire and cross the sweep interval without ever reading "ghost".
        advance(Duration::from_secs(60)).await;

        // A write on a different key triggers the sweep, which drops the stale entry.
        store
            .set("live", b"y".to_vec(), Duration::from_secs(300))
            .await?;
        assert_eq!(
            store.entry_count().await,
            1,
            "only the live key should remain after the sweep"
        );
        assert_eq!(store.get("live").await?, Some(b"y".to_vec()));
        assert_eq!(store.get("ghost").await?, None);

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_leases_enforce_limit_idempotency_release_and_ttl() -> Result<()> {
        // Pins: shared admission never exceeds its cap, a retry with the same
        // durable lease id does not consume another slot, terminal release is
        // idempotent, and crash-leaked slots become available after TTL expiry.
        let store = MemoryRuntimeCacheStore::default();
        let ttl = Duration::from_secs(10);

        let first = store
            .try_acquire_bounded_lease("turns", "session-a", 2, ttl)
            .await?;
        assert!(first.acquired);
        assert_eq!(first.live, 1);

        let replay = store
            .try_acquire_bounded_lease("turns", "session-a", 2, ttl)
            .await?;
        assert_eq!(replay, first);
        assert!(
            store
                .try_acquire_bounded_lease("turns", "session-b", 2, ttl)
                .await?
                .acquired
        );
        let saturated = store
            .try_acquire_bounded_lease("turns", "session-c", 2, ttl)
            .await?;
        assert!(!saturated.acquired);
        assert_eq!(saturated.live, 2);

        assert_eq!(store.release_bounded_lease("turns", "session-a").await?, 1);
        assert!(
            store
                .try_acquire_bounded_lease("turns", "session-c", 2, ttl)
                .await?
                .acquired
        );
        advance(Duration::from_secs(11)).await;
        let after_expiry = store
            .try_acquire_bounded_lease("turns", "session-d", 2, ttl)
            .await?;
        assert!(after_expiry.acquired);
        assert_eq!(after_expiry.live, 1);

        Ok(())
    }

    /// The behavior every coordination backend must agree on, written once.
    ///
    /// Backends that disagree here make the same deployment behave differently
    /// depending on which cache it happens to run against, which is exactly the
    /// class of bug a per-backend test suite hides. Uses real (short) durations
    /// so it can run unchanged against an out-of-process store.
    pub(crate) async fn assert_shared_coordination_conformance(
        store: &dyn RuntimeCacheStore,
        prefix: &str,
    ) -> Result<()> {
        let ttl = Duration::from_secs(30);
        let window = Duration::from_secs(30);
        let pace_key = format!("{prefix}:pace");
        let cooldown_key = format!("{prefix}:cooldown");
        let budget_key = format!("{prefix}:budget");

        // 600/min = 10 tokens/sec. The first minute of budget is available as a
        // burst; the next permit must then report a positive refill wait.
        assert!(
            store
                .try_consume_rate_tokens(&pace_key, 600, 600, ttl)
                .await?
                .admitted
        );
        let denied = store
            .try_consume_rate_tokens(&pace_key, 600, 10, ttl)
            .await?;
        assert!(
            !denied.admitted,
            "a drained bucket must deny further permits"
        );
        assert!(
            denied.retry_after > Duration::ZERO,
            "a denial must carry the wait that makes it actionable"
        );

        // Cooldowns move forward only, and a separate reader observes them.
        assert_eq!(
            store.cooldown_remaining(&cooldown_key).await?,
            Duration::ZERO
        );
        let long = store
            .extend_cooldown(&cooldown_key, Duration::from_secs(20))
            .await?;
        assert!(long > Duration::from_secs(15));
        let shortened = store
            .extend_cooldown(&cooldown_key, Duration::from_secs(1))
            .await?;
        assert!(
            shortened > Duration::from_secs(15),
            "a shorter cooldown must not shorten an active longer pause"
        );
        assert!(store.cooldown_remaining(&cooldown_key).await? > Duration::from_secs(15));

        // The retry budget allows the floor, then refuses until volume grows.
        assert_eq!(store.note_windowed_request(&budget_key, window).await?, 1);
        for _ in 0..4 {
            assert!(
                store
                    .try_consume_retry_budget(&budget_key, window, 20, 4)
                    .await?
                    .allowed
            );
        }
        let exhausted = store
            .try_consume_retry_budget(&budget_key, window, 20, 4)
            .await?;
        assert!(
            !exhausted.allowed,
            "the floor must bound low-volume retries"
        );
        assert_eq!(exhausted.requests, 1);
        assert_eq!(exhausted.retries, 4);

        // Growing request volume raises the allowance above the floor.
        for _ in 0..99 {
            store.note_windowed_request(&budget_key, window).await?;
        }
        assert!(
            store
                .try_consume_retry_budget(&budget_key, window, 20, 4)
                .await?
                .allowed,
            "20% of 100 requests must exceed the 4 retries already spent"
        );

        store.delete(&pace_key).await?;
        store.delete(&cooldown_key).await?;
        store.delete(&budget_key).await?;
        Ok(())
    }

    #[tokio::test]
    async fn memory_backend_meets_the_shared_coordination_contract() -> Result<()> {
        // Pins: the in-memory backend satisfies the same coordination contract the
        // Redis backend is held to by its live test.
        assert_shared_coordination_conformance(&MemoryRuntimeCacheStore::new(), "conformance").await
    }

    #[tokio::test(start_paused = true)]
    async fn shared_rate_tokens_pace_one_bucket_and_report_the_refill_wait() -> Result<()> {
        // Pins: one shared per-minute bucket admits a full minute of burst, then
        // denies further permits with the exact refill wait rather than a guess,
        // and admits again once that wait has elapsed. A demand larger than the
        // whole bucket drains it instead of deadlocking.
        let store = MemoryRuntimeCacheStore::default();
        let ttl = Duration::from_secs(300);

        // 120/min = 2 tokens/sec; the initial burst is the whole minute.
        let burst = store.try_consume_rate_tokens("pace", 120, 120, ttl).await?;
        assert!(burst.admitted);
        assert_eq!(burst.retry_after, Duration::ZERO);

        let denied = store.try_consume_rate_tokens("pace", 120, 2, ttl).await?;
        assert!(
            !denied.admitted,
            "a drained shared bucket must deny permits"
        );
        assert_eq!(
            denied.retry_after,
            Duration::from_secs(1),
            "2 permits at 2 tokens/sec is exactly one second of refill"
        );

        advance(Duration::from_secs(1)).await;
        assert!(
            store
                .try_consume_rate_tokens("pace", 120, 2, ttl)
                .await?
                .admitted,
            "the refilled bucket must admit the same permits"
        );

        // An oversized single demand is clamped to capacity: it drains rather
        // than waiting for a refill that can never satisfy it.
        assert!(
            store
                .try_consume_rate_tokens("oversized", 10, 1_000, ttl)
                .await?
                .admitted
        );

        let error = store
            .try_consume_rate_tokens("zero", 0, 1, ttl)
            .await
            .expect_err("a zero limit is a configuration error, not an unbounded bucket");
        assert!(matches!(
            error,
            moa_core::error::MoaError::ValidationError(_)
        ));

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shared_cooldown_only_moves_forward_and_clears_on_expiry() -> Result<()> {
        // Pins: a shared 429 cooldown takes the longest deadline any replica
        // recorded (a later short cooldown cannot shorten it) and reports no
        // remaining pause once it elapses.
        let store = MemoryRuntimeCacheStore::default();

        assert_eq!(store.cooldown_remaining("cool").await?, Duration::ZERO);

        let long = store
            .extend_cooldown("cool", Duration::from_secs(10))
            .await?;
        assert_eq!(long, Duration::from_secs(10));

        let short = store
            .extend_cooldown("cool", Duration::from_secs(1))
            .await?;
        assert_eq!(
            short,
            Duration::from_secs(10),
            "a shorter cooldown must not shorten an active longer pause"
        );

        advance(Duration::from_secs(4)).await;
        assert_eq!(
            store.cooldown_remaining("cool").await?,
            Duration::from_secs(6)
        );

        advance(Duration::from_secs(7)).await;
        assert_eq!(
            store.cooldown_remaining("cool").await?,
            Duration::ZERO,
            "the pause must clear once the deadline passes"
        );

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shared_retry_budget_tracks_fleet_volume_and_rotates_its_window() -> Result<()> {
        // Pins: the shared budget allows the floor at low volume, grows to the
        // configured percentage of fleet-wide request volume, and resets when
        // the sliding window rotates.
        let store = MemoryRuntimeCacheStore::default();
        let window = Duration::from_secs(60);

        assert_eq!(store.note_windowed_request("budget", window).await?, 1);
        for _ in 0..8 {
            assert!(
                store
                    .try_consume_retry_budget("budget", window, 20, 8)
                    .await?
                    .allowed,
                "retries within the floor are allowed at low volume"
            );
        }
        assert!(
            !store
                .try_consume_retry_budget("budget", window, 20, 8)
                .await?
                .allowed,
            "the floor must stop further retries until request volume grows"
        );

        // Fleet volume grows: 100 requests at 20% allows 20 retries total, so 12
        // more beyond the 8 already spent.
        for _ in 0..99 {
            store.note_windowed_request("budget", window).await?;
        }
        let mut allowed = 0;
        for _ in 0..50 {
            if store
                .try_consume_retry_budget("budget", window, 20, 8)
                .await?
                .allowed
            {
                allowed += 1;
            }
        }
        assert_eq!(
            allowed, 12,
            "20% of 100 fleet requests minus the 8 already consumed"
        );

        advance(Duration::from_secs(61)).await;
        let rotated = store
            .try_consume_retry_budget("budget", window, 20, 8)
            .await?;
        assert!(rotated.allowed, "a rotated window restores the floor");
        assert_eq!(rotated.requests, 0, "the rotated window starts empty");
        assert_eq!(rotated.retries, 1);

        Ok(())
    }

    #[tokio::test]
    async fn a_cache_without_coordination_support_fails_closed_on_every_control() {
        // Pins: the trait defaults refuse to answer coordination questions, so a
        // cache that only implements plain key/value operations can never be
        // mistaken for a fleet-wide pacer, cooldown, or retry budget.
        struct KeyValueOnlyCache;

        #[async_trait::async_trait]
        impl RuntimeCacheStore for KeyValueOnlyCache {
            async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
                Ok(None)
            }
            async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
                Ok(())
            }
            async fn delete(&self, _key: &str) -> Result<()> {
                Ok(())
            }
            async fn compare_and_set(
                &self,
                _key: &str,
                _expected: Option<&[u8]>,
                _value: Vec<u8>,
                _ttl: Duration,
            ) -> Result<bool> {
                Ok(true)
            }
            async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
                Ok(())
            }
        }

        let store = KeyValueOnlyCache;
        let ttl = Duration::from_secs(1);
        assert!(
            store
                .try_consume_rate_tokens("k", 60, 1, ttl)
                .await
                .is_err()
        );
        assert!(store.extend_cooldown("k", ttl).await.is_err());
        assert!(store.cooldown_remaining("k").await.is_err());
        assert!(store.note_windowed_request("k", ttl).await.is_err());
        assert!(
            store
                .try_consume_retry_budget("k", ttl, 20, 8)
                .await
                .is_err()
        );
        assert!(
            store
                .try_acquire_bounded_lease("k", "lease", 1, ttl)
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn memory_compare_and_set_matches_absent_and_exact_values() -> Result<()> {
        // Pins: compare-and-set only mutates when the expected bytes match current cache state.
        let store = MemoryRuntimeCacheStore::default();

        assert!(
            store
                .compare_and_set("slot", None, b"one".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert!(
            !store
                .compare_and_set("slot", None, b"two".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"one".to_vec()));

        assert!(
            !store
                .compare_and_set(
                    "slot",
                    Some(b"wrong"),
                    b"two".to_vec(),
                    Duration::from_secs(10),
                )
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"one".to_vec()));

        assert!(
            store
                .compare_and_set(
                    "slot",
                    Some(b"one"),
                    b"two".to_vec(),
                    Duration::from_secs(1),
                )
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"two".to_vec()));

        advance(Duration::from_secs(2)).await;
        assert!(
            store
                .compare_and_set("slot", None, b"three".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"three".to_vec()));

        Ok(())
    }

    #[test]
    fn backend_resolution_matrix_fails_closed_without_opt_in() {
        // Pins: `auto` only resolves to the process-local memory backend with an explicit opt-in;
        // otherwise it fails closed rather than silently degrading fleet coordination. Driven
        // through the pure resolver so the opt-in flag is deterministic (no env/config races).
        use ResolvedRuntimeCacheBackend::{Memory, Redis};

        // Explicit backends are honored regardless of URL/opt-in.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Redis, false, false).unwrap(),
            Redis
        );
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Memory, false, false).unwrap(),
            Memory
        );
        // `auto` prefers Redis when a URL is present.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, true, false).unwrap(),
            Redis
        );
        // `auto` without a URL yields memory only with an explicit opt-in.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, false, true).unwrap(),
            Memory
        );
        // `auto` without a URL and without opt-in fails closed with an actionable error.
        let error = resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, false, false)
            .expect_err("auto without redis url or opt-in must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("redis_url") && message.contains(super::MEMORY_BACKEND_OPT_IN_ENV),
            "error should tell the operator how to fix it: {message}"
        );
    }

    #[test]
    fn select_backend_reads_config_and_prefers_redis() {
        // Pins: the config-driven selector honors an explicit URL and explicit backends without
        // depending on the opt-in environment flag.
        let auto_with_url = RuntimeCacheConfig {
            redis_url: Some("redis://cache.example:6379/0".to_string()),
            ..RuntimeCacheConfig::default()
        };
        assert_eq!(
            select_runtime_cache_backend(&auto_with_url).unwrap(),
            ResolvedRuntimeCacheBackend::Redis
        );

        let explicit_memory = RuntimeCacheConfig {
            backend: RuntimeCacheBackend::Memory,
            redis_url: None,
        };
        assert_eq!(
            select_runtime_cache_backend(&explicit_memory).unwrap(),
            ResolvedRuntimeCacheBackend::Memory
        );

        let explicit_redis = RuntimeCacheConfig {
            backend: RuntimeCacheBackend::Redis,
            redis_url: None,
        };
        assert_eq!(
            select_runtime_cache_backend(&explicit_redis).unwrap(),
            ResolvedRuntimeCacheBackend::Redis
        );
    }
}
