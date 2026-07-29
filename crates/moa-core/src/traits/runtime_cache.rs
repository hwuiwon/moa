//! Runtime cache trait for ephemeral coordination state.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::Result;

/// Result of one atomic bounded-lease admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedLeaseDecision {
    /// Whether this lease owns a slot after the operation.
    pub acquired: bool,
    /// Number of live leases observed after expired leases were pruned.
    pub live: usize,
}

/// Result of one atomic shared token-bucket admission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateTokenDecision {
    /// Whether the requested permits were consumed by this attempt.
    pub admitted: bool,
    /// How long until the requested permits refill; zero when admitted.
    ///
    /// Bounded by one bucket refill period, so a caller that sleeps for this
    /// duration and retries cannot be parked indefinitely by a hostile value.
    pub retry_after: Duration,
}

/// Result of one atomic shared retry-budget consumption attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBudgetDecision {
    /// Whether one unit of retry budget was consumed by this attempt.
    pub allowed: bool,
    /// Requests observed in the current window across the fleet.
    pub requests: u64,
    /// Retries observed in the current window across the fleet, including this
    /// one when `allowed` is true.
    pub retries: u64,
}

/// Ephemeral byte-value cache used for runtime coordination.
#[async_trait]
pub trait RuntimeCacheStore: Send + Sync {
    /// Loads one cache value when the key exists and has not expired.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Stores one cache value with a time-to-live.
    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()>;

    /// Removes one cache value when present.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Replaces a cache value when the current value matches the expected bytes.
    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool>;

    /// Updates the expiration for an existing unexpired cache value.
    async fn expire(&self, key: &str, ttl: Duration) -> Result<()>;

    /// Atomically acquires or renews one idempotent lease in a bounded shared set.
    ///
    /// Implementations used for distributed coordination must override this
    /// operation with a backend-native atomic primitive. The default fails
    /// closed so a cache implementation cannot silently provide process-local
    /// or racy admission semantics.
    async fn try_acquire_bounded_lease(
        &self,
        _key: &str,
        _lease_id: &str,
        _limit: usize,
        _ttl: Duration,
    ) -> Result<BoundedLeaseDecision> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support atomic bounded leases".to_string(),
        ))
    }

    /// Atomically releases one idempotent lease from a bounded shared set.
    ///
    /// Returns the number of live leases remaining after expiry pruning.
    async fn release_bounded_lease(&self, _key: &str, _lease_id: &str) -> Result<usize> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support atomic bounded leases".to_string(),
        ))
    }

    /// Atomically consumes `permits` from a shared per-minute token bucket.
    ///
    /// The bucket holds `limit_per_min` tokens and refills continuously at
    /// `limit_per_min / 60` tokens per second, so one shared provider quota is
    /// paced once for the whole fleet instead of once per replica. A demand
    /// larger than the whole bucket is clamped to its capacity so an oversized
    /// single request drains the bucket rather than deadlocking.
    ///
    /// Implementations used for distributed coordination must override this with
    /// a backend-native atomic primitive. The default fails closed so a cache
    /// implementation cannot silently provide process-local pacing.
    async fn try_consume_rate_tokens(
        &self,
        _key: &str,
        _limit_per_min: u32,
        _permits: u32,
        _ttl: Duration,
    ) -> Result<RateTokenDecision> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support atomic shared rate tokens".to_string(),
        ))
    }

    /// Extends a shared cooldown deadline and returns the remaining cooldown.
    ///
    /// The stored deadline only ever moves forward (`max` of the current and
    /// requested deadlines), so a late writer with a short cooldown cannot
    /// shorten a longer pause another replica already recorded.
    async fn extend_cooldown(&self, _key: &str, _cooldown: Duration) -> Result<Duration> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support shared cooldowns".to_string(),
        ))
    }

    /// Returns the remaining shared cooldown, or [`Duration::ZERO`] when none.
    async fn cooldown_remaining(&self, _key: &str) -> Result<Duration> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support shared cooldowns".to_string(),
        ))
    }

    /// Counts one outbound request in the shared sliding retry-budget window.
    ///
    /// Returns the request count observed in the current window after the
    /// increment. The window rotates when `window` has elapsed since it opened.
    async fn note_windowed_request(&self, _key: &str, _window: Duration) -> Result<u64> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support shared retry budgets".to_string(),
        ))
    }

    /// Atomically consumes one unit of the shared retry budget when available.
    ///
    /// The budget is `max(requests * budget_percent / 100, budget_floor)` over
    /// the current window, so retries across the fleet stay a bounded fraction
    /// of fleet request volume and a burst of rate-limited calls cannot amplify
    /// into a retry storm multiplied by the replica count.
    async fn try_consume_retry_budget(
        &self,
        _key: &str,
        _window: Duration,
        _budget_percent: u32,
        _budget_floor: u64,
    ) -> Result<RetryBudgetDecision> {
        Err(crate::error::MoaError::ConfigError(
            "runtime cache does not support shared retry budgets".to_string(),
        ))
    }
}
