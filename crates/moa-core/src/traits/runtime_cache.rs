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
}
