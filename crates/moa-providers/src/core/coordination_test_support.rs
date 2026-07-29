//! Shared coordination-store doubles for the fleet-coordination unit tests.
//!
//! The distributed provider controls all branch on "the coordination store
//! answered" versus "the coordination store failed", so every one of them needs
//! the same failing double. Keeping one here means a policy test cannot
//! accidentally pass because a local copy of the double failed differently.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::{
    BoundedLeaseDecision, RateTokenDecision, RetryBudgetDecision, RuntimeCacheStore,
};

/// A coordination store where every operation fails.
///
/// It overrides the coordination operations explicitly rather than relying on
/// the trait's fail-closed defaults, so these tests exercise the "store is
/// reachable but erroring" path a real outage produces.
pub(crate) struct FailingStore;

/// A coordination store whose operations never complete.
pub(crate) struct HangingStore;

/// Coordination store that admits pacing and counts exact token consumptions.
#[derive(Default)]
pub(crate) struct CountingPacingStore {
    calls: AtomicUsize,
}

impl CountingPacingStore {
    /// Returns the number of shared pacing operations observed.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn unavailable<T>() -> Result<T> {
    Err(MoaError::StorageError(
        "test coordination store unavailable".to_string(),
    ))
}

#[async_trait]
impl RuntimeCacheStore for FailingStore {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        unavailable()
    }

    async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
        unavailable()
    }

    async fn delete(&self, _key: &str) -> Result<()> {
        unavailable()
    }

    async fn compare_and_set(
        &self,
        _key: &str,
        _expected: Option<&[u8]>,
        _value: Vec<u8>,
        _ttl: Duration,
    ) -> Result<bool> {
        unavailable()
    }

    async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
        unavailable()
    }

    async fn try_acquire_bounded_lease(
        &self,
        _key: &str,
        _lease_id: &str,
        _limit: usize,
        _ttl: Duration,
    ) -> Result<BoundedLeaseDecision> {
        unavailable()
    }

    async fn release_bounded_lease(&self, _key: &str, _lease_id: &str) -> Result<usize> {
        unavailable()
    }

    async fn try_consume_rate_tokens(
        &self,
        _key: &str,
        _limit_per_min: u32,
        _permits: u32,
        _ttl: Duration,
    ) -> Result<RateTokenDecision> {
        unavailable()
    }

    async fn extend_cooldown(&self, _key: &str, _cooldown: Duration) -> Result<Duration> {
        unavailable()
    }

    async fn cooldown_remaining(&self, _key: &str) -> Result<Duration> {
        unavailable()
    }

    async fn note_windowed_request(&self, _key: &str, _window: Duration) -> Result<u64> {
        unavailable()
    }

    async fn try_consume_retry_budget(
        &self,
        _key: &str,
        _window: Duration,
        _budget_percent: u32,
        _budget_floor: u64,
    ) -> Result<RetryBudgetDecision> {
        unavailable()
    }
}

#[async_trait]
impl RuntimeCacheStore for HangingStore {
    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        std::future::pending().await
    }

    async fn set(&self, _key: &str, _value: Vec<u8>, _ttl: Duration) -> Result<()> {
        std::future::pending().await
    }

    async fn delete(&self, _key: &str) -> Result<()> {
        std::future::pending().await
    }

    async fn compare_and_set(
        &self,
        _key: &str,
        _expected: Option<&[u8]>,
        _value: Vec<u8>,
        _ttl: Duration,
    ) -> Result<bool> {
        std::future::pending().await
    }

    async fn expire(&self, _key: &str, _ttl: Duration) -> Result<()> {
        std::future::pending().await
    }

    async fn cooldown_remaining(&self, _key: &str) -> Result<Duration> {
        std::future::pending().await
    }
}

#[async_trait]
impl RuntimeCacheStore for CountingPacingStore {
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

    async fn try_consume_rate_tokens(
        &self,
        _key: &str,
        _limit_per_min: u32,
        _permits: u32,
        _ttl: Duration,
    ) -> Result<RateTokenDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(RateTokenDecision {
            admitted: true,
            retry_after: Duration::ZERO,
        })
    }
}
