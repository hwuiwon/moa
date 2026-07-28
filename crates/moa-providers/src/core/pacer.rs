//! Request/input pacing for provider API rate limits.
//!
//! Providers such as Cohere document per-minute ceilings in different units:
//! embeddings are limited by *inputs* per minute while rerank/chat are limited
//! by *requests* per minute. [`RatePacer`] applies a token bucket to each
//! configured dimension so a busy caller stays under those ceilings before the
//! HTTP request is sent, complementing (not replacing) any concurrency window a
//! provider already applies.
//!
//! # Process-local versus fleet-wide
//!
//! With no shared quota attached, the buckets are process-local: a fleet sharing
//! one API key must then give each instance its fraction of the documented
//! budget. Attaching a shared quota
//! ([`with_shared_quota`](RatePacer::with_shared_quota)) moves the buckets into
//! the runtime coordination store, keyed per provider, opaque credential
//! identity, model, and rate class, so the whole fleet spends one documented
//! per-minute budget exactly once. The local buckets remain as the bounded
//! fallback for a coordination failure under `bounded_degraded`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use moa_config::{CoordinationFailurePolicy, ProviderPacingConfig};
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
use tokio::time::{Instant, sleep};

use super::concurrency_factory::{
    CoordinatedControl, QuotaIdentity, record_coordination_degraded, record_coordination_rejected,
};

const SECONDS_PER_MINUTE: f64 = 60.0;

/// Rate class label for the requests-per-minute dimension.
const CLASS_REQUESTS: &str = "requests";
/// Rate class label for the inputs-per-minute dimension.
const CLASS_INPUTS: &str = "inputs";

/// Floor on one distributed pacing wait, so a rounded-down refill estimate
/// cannot turn the wait loop into a busy loop against the store.
const MIN_SHARED_WAIT: Duration = Duration::from_millis(5);

/// Per-minute rate limits for one provider endpoint.
///
/// Each dimension is optional; a `None` dimension is unbounded. A fully-`None`
/// config yields a pacer that never blocks, which is the default for providers
/// whose per-tier limits are not modeled here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacerConfig {
    /// Maximum requests per minute, or `None` for no request-rate bound.
    pub max_requests_per_min: Option<u32>,
    /// Maximum inputs per minute, or `None` for no input-rate bound.
    pub max_inputs_per_min: Option<u32>,
}

impl PacerConfig {
    /// A configuration that imposes no pacing.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            max_requests_per_min: None,
            max_inputs_per_min: None,
        }
    }

    /// A request-per-minute-only configuration (e.g. Cohere rerank).
    #[must_use]
    pub const fn requests_per_min(limit: u32) -> Self {
        Self {
            max_requests_per_min: Some(limit),
            max_inputs_per_min: None,
        }
    }

    /// An input-per-minute-only configuration (e.g. Cohere embed).
    #[must_use]
    pub const fn inputs_per_min(limit: u32) -> Self {
        Self {
            max_requests_per_min: None,
            max_inputs_per_min: Some(limit),
        }
    }

    /// Returns whether any dimension is bounded.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.max_requests_per_min.is_some() || self.max_inputs_per_min.is_some()
    }
}

/// A cloneable per-endpoint pacer; clones share one set of token buckets.
#[derive(Clone)]
pub(crate) struct RatePacer {
    limits: PacerConfig,
    inner: Arc<PacerInner>,
    shared: Option<Arc<SharedQuota>>,
}

struct PacerInner {
    state: Mutex<PacerState>,
}

struct PacerState {
    requests: Option<Bucket>,
    inputs: Option<Bucket>,
}

/// Fleet-shared pacing state for one credential's quota.
struct SharedQuota {
    store: Arc<dyn RuntimeCacheStore>,
    identity: QuotaIdentity,
    config: ProviderPacingConfig,
    on_failure: CoordinationFailurePolicy,
}

impl RatePacer {
    /// Builds a process-local pacer from a per-minute limit configuration.
    pub(crate) fn new(limits: PacerConfig) -> Self {
        let now = Instant::now();
        Self {
            limits,
            inner: Arc::new(PacerInner {
                state: Mutex::new(PacerState {
                    requests: limits
                        .max_requests_per_min
                        .map(|limit| Bucket::new(limit, now)),
                    inputs: limits
                        .max_inputs_per_min
                        .map(|limit| Bucket::new(limit, now)),
                }),
            }),
            shared: None,
        }
    }

    /// Moves pacing into the coordination store for one credential's quota.
    ///
    /// A `None` store, or a pacer with no bounded dimension, leaves pacing
    /// process-local: there is no fleet budget to divide.
    pub(crate) fn with_shared_quota(
        mut self,
        store: Option<Arc<dyn RuntimeCacheStore>>,
        identity: QuotaIdentity,
        config: ProviderPacingConfig,
        on_failure: CoordinationFailurePolicy,
    ) -> Self {
        self.shared = store.filter(|_| self.limits.is_enabled()).map(|store| {
            Arc::new(SharedQuota {
                store,
                identity,
                config,
                on_failure,
            })
        });
        self
    }

    /// Waits until both configured dimensions can admit `requests` and `inputs`
    /// for `model`, then consumes that budget.
    ///
    /// Unbounded dimensions are ignored, so a fully-disabled pacer returns
    /// immediately without touching the clock or the store.
    ///
    /// # Errors
    ///
    /// Returns a typed rate-limit error only when shared pacing cannot reach the
    /// coordination store and the configured policy is `fail_closed`.
    pub(crate) async fn acquire(&self, model: &str, requests: u32, inputs: u32) -> Result<()> {
        match self.shared.as_ref() {
            Some(shared) => self.acquire_shared(shared, model, requests, inputs).await,
            None => {
                self.acquire_local(requests, inputs).await;
                Ok(())
            }
        }
    }

    /// Spends the fleet-wide budget through the coordination store.
    ///
    /// Each dimension is a separate atomic bucket operation, so a dimension that
    /// has already been granted is not re-consumed while waiting for the other.
    async fn acquire_shared(
        &self,
        shared: &SharedQuota,
        model: &str,
        requests: u32,
        inputs: u32,
    ) -> Result<()> {
        let started = Instant::now();
        let ttl = Duration::from_millis(shared.config.state_ttl_ms);
        let max_wait = Duration::from_millis(shared.config.max_pacing_wait_ms);
        let mut pending: Vec<(String, u32, u32)> = Vec::with_capacity(2);
        if let Some(limit) = self.limits.max_requests_per_min.filter(|_| requests > 0) {
            pending.push((
                shared.identity.key("pace", model, CLASS_REQUESTS),
                limit,
                requests,
            ));
        }
        if let Some(limit) = self.limits.max_inputs_per_min.filter(|_| inputs > 0) {
            pending.push((
                shared.identity.key("pace", model, CLASS_INPUTS),
                limit,
                inputs,
            ));
        }

        while !pending.is_empty() {
            let mut wait = Duration::ZERO;
            let mut still_pending = Vec::with_capacity(pending.len());
            for (key, limit, permits) in pending {
                match shared
                    .store
                    .try_consume_rate_tokens(&key, limit, permits, ttl)
                    .await
                {
                    Ok(decision) if decision.admitted => {}
                    Ok(decision) => {
                        wait = wait.max(decision.retry_after);
                        still_pending.push((key, limit, permits));
                    }
                    // A store failure is decided by policy on the spot. Retrying
                    // a broken store in the admission path is the storm this
                    // control exists to prevent.
                    Err(error) => {
                        return self
                            .on_shared_failure(shared, &error, started.elapsed(), requests, inputs)
                            .await;
                    }
                }
            }
            pending = still_pending;
            if pending.is_empty() {
                break;
            }
            sleep(wait.clamp(MIN_SHARED_WAIT, max_wait)).await;
        }
        Ok(())
    }

    /// Applies the coordination-failure policy to a shared pacing failure.
    async fn on_shared_failure(
        &self,
        shared: &SharedQuota,
        error: &MoaError,
        elapsed: Duration,
        requests: u32,
        inputs: u32,
    ) -> Result<()> {
        if shared.on_failure.rejects_admission() {
            record_coordination_rejected(
                shared.identity.provider(),
                CoordinatedControl::Pacing,
                error,
            );
            return Err(MoaError::RateLimited {
                retries: 0,
                message: format!(
                    "provider pacing coordination is unavailable and the coordination-failure \
                     policy is fail_closed: {error}"
                ),
            });
        }
        record_coordination_degraded(
            shared.identity.provider(),
            CoordinatedControl::Pacing,
            elapsed,
            error,
        );
        self.acquire_local(requests, inputs).await;
        Ok(())
    }

    /// Spends this process's own token buckets.
    async fn acquire_local(&self, requests: u32, inputs: u32) {
        loop {
            let wait = {
                let mut state = self.lock_state();
                if state.requests.is_none() && state.inputs.is_none() {
                    return;
                }
                let now = Instant::now();
                let mut wait = Duration::ZERO;
                if let Some(bucket) = state.requests.as_mut() {
                    bucket.refill(now);
                    wait = wait.max(bucket.deficit_wait(f64::from(requests)));
                }
                if let Some(bucket) = state.inputs.as_mut() {
                    bucket.refill(now);
                    wait = wait.max(bucket.deficit_wait(f64::from(inputs)));
                }
                if wait.is_zero() {
                    if let Some(bucket) = state.requests.as_mut() {
                        bucket.consume(f64::from(requests));
                    }
                    if let Some(bucket) = state.inputs.as_mut() {
                        bucket.consume(f64::from(inputs));
                    }
                    return;
                }
                wait
            };
            sleep(wait).await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PacerState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A continuously-refilling token bucket for one rate dimension.
struct Bucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(limit: u32, now: Instant) -> Self {
        let capacity = f64::from(limit.max(1));
        Self {
            capacity,
            tokens: capacity,
            refill_per_sec: capacity / SECONDS_PER_MINUTE,
            last_refill: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Returns how long until `need` tokens are available; a single demand larger
    /// than capacity is clamped so it drains the bucket rather than deadlocking.
    fn deficit_wait(&self, need: f64) -> Duration {
        let need = need.min(self.capacity);
        if self.tokens >= need {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((need - self.tokens) / self.refill_per_sec)
        }
    }

    fn consume(&mut self, need: f64) {
        self.tokens -= need.min(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use moa_config::ConcurrencyScope;
    use moa_runtime_store::MemoryRuntimeCacheStore;

    use super::*;

    const MODEL: &str = "test-model";

    fn shared_pacing_config() -> ProviderPacingConfig {
        ProviderPacingConfig {
            scope: ConcurrencyScope::Global,
            ..ProviderPacingConfig::default()
        }
    }

    fn shared_pacer(
        limits: PacerConfig,
        store: Arc<dyn RuntimeCacheStore>,
        on_failure: CoordinationFailurePolicy,
    ) -> RatePacer {
        RatePacer::new(limits).with_shared_quota(
            Some(store),
            QuotaIdentity::new("cohere", "shared-credential"),
            shared_pacing_config(),
            on_failure,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn pacer_paces_inputs_per_minute_after_the_bucket_drains() {
        // Pins: once the input bucket is drained, further inputs wait for refill
        // at limit/60 per second before proceeding.
        let pacer = RatePacer::new(PacerConfig::inputs_per_min(120));
        // Drain the full minute of capacity up front (initial burst is allowed).
        pacer.acquire(MODEL, 0, 120).await.expect("local pacing");

        let waiter = pacer.clone();
        let task = tokio::spawn(async move { waiter.acquire(MODEL, 0, 2).await });
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "a drained input bucket must make the next inputs wait for refill"
        );

        // 120/min = 2 tokens/sec, so 2 inputs need one second of refill.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        task.await
            .expect("acquire should complete after refill")
            .expect("local pacing never fails");
    }

    #[tokio::test(start_paused = true)]
    async fn pacer_enforces_the_stricter_of_two_dimensions() {
        // Pins: both request and input dimensions must be satisfied; the slower
        // one governs. Here inputs refill slower than requests.
        let pacer = RatePacer::new(PacerConfig {
            max_requests_per_min: Some(600), // 10/sec
            max_inputs_per_min: Some(60),    // 1/sec
        });
        pacer.acquire(MODEL, 600, 60).await.expect("drain both");

        let waiter = pacer.clone();
        // 1 request refills in 0.1s, but 1 input needs 1s; the input bound wins.
        let task = tokio::spawn(async move { waiter.acquire(MODEL, 1, 1).await });
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "the input dimension should still be gating at T+0.5s"
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        task.await
            .expect("acquire should complete once inputs refill")
            .expect("local pacing never fails");
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_pacer_never_blocks_even_for_large_demand() {
        // Pins: an unbounded pacer is a no-op regardless of the requested budget.
        let pacer = RatePacer::new(PacerConfig::disabled());
        pacer
            .acquire(MODEL, 1_000_000, 1_000_000)
            .await
            .expect("no-op");
        pacer
            .acquire(MODEL, 1_000_000, 1_000_000)
            .await
            .expect("no-op");
    }

    #[tokio::test(start_paused = true)]
    async fn demand_larger_than_capacity_drains_without_deadlock() {
        // Pins: a single request bigger than the whole per-minute budget still
        // makes progress instead of waiting forever.
        let pacer = RatePacer::new(PacerConfig::inputs_per_min(100));
        pacer.acquire(MODEL, 0, 250).await.expect("clamped demand");
    }

    #[tokio::test(start_paused = true)]
    async fn two_replicas_spend_one_shared_per_minute_budget() {
        // Pins: the distinguishing behavior of this task. Two pacers that share a
        // coordination store spend ONE 120/min budget between them: replica A's
        // burst drains the fleet bucket, so replica B — which has its own full
        // local bucket and would sail through under process-local pacing — is
        // paced until the shared bucket refills.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let replica_a = shared_pacer(
            PacerConfig::inputs_per_min(120),
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );
        let replica_b = shared_pacer(
            PacerConfig::inputs_per_min(120),
            Arc::clone(&store),
            CoordinationFailurePolicy::BoundedDegraded,
        );

        replica_a
            .acquire(MODEL, 0, 120)
            .await
            .expect("replica A drains the shared minute");

        let waiter = replica_b.clone();
        let task = tokio::spawn(async move { waiter.acquire(MODEL, 0, 2).await });
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "replica B must be paced by the budget replica A already spent"
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        task.await
            .expect("replica B proceeds once the shared bucket refills")
            .expect("shared pacing succeeds");
    }

    #[tokio::test(start_paused = true)]
    async fn independent_replicas_each_get_a_full_budget_without_coordination() {
        // Pins: the negative control for the test above. With no shared quota the
        // same two pacers each hold their own full 120/min bucket, so replica B is
        // NOT paced by replica A. Without this, the shared test above could pass
        // for reasons unrelated to coordination.
        let replica_a = RatePacer::new(PacerConfig::inputs_per_min(120));
        let replica_b = RatePacer::new(PacerConfig::inputs_per_min(120));

        replica_a.acquire(MODEL, 0, 120).await.expect("drain A");

        let waiter = replica_b.clone();
        let task = tokio::spawn(async move { waiter.acquire(MODEL, 0, 2).await });
        tokio::task::yield_now().await;
        assert!(
            task.is_finished(),
            "an uncoordinated replica must not be paced by another replica's usage"
        );
        task.await.expect("join").expect("local pacing");
    }

    #[tokio::test(start_paused = true)]
    async fn shared_pacing_separates_models_and_rate_classes() {
        // Pins: the shared bucket is keyed per model and rate class, so draining
        // one model's budget does not pace a different model on the same key.
        let store: Arc<dyn RuntimeCacheStore> = Arc::new(MemoryRuntimeCacheStore::new());
        let pacer = shared_pacer(
            PacerConfig::inputs_per_min(120),
            store,
            CoordinationFailurePolicy::BoundedDegraded,
        );

        pacer.acquire("model-a", 0, 120).await.expect("drain a");

        let waiter = pacer.clone();
        let task = tokio::spawn(async move { waiter.acquire("model-b", 0, 120).await });
        tokio::task::yield_now().await;
        assert!(
            task.is_finished(),
            "a different model must draw from its own shared bucket"
        );
        task.await.expect("join").expect("shared pacing");
    }

    #[tokio::test(start_paused = true)]
    async fn shared_pacing_degrades_to_the_local_bucket_when_the_store_fails() {
        // Pins: bounded_degraded keeps calls flowing on a coordination failure and
        // still paces them against this replica's own bucket.
        let pacer = shared_pacer(
            PacerConfig::inputs_per_min(120),
            Arc::new(crate::core::coordination_test_support::FailingStore),
            CoordinationFailurePolicy::BoundedDegraded,
        );

        pacer
            .acquire(MODEL, 0, 120)
            .await
            .expect("degrades to the local bucket rather than failing");

        // The local bucket is now drained, so the degraded path still paces.
        let waiter = pacer.clone();
        let task = tokio::spawn(async move { waiter.acquire(MODEL, 0, 2).await });
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "the degraded fallback must still bound this replica's rate"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        task.await.expect("join").expect("degraded pacing");
    }

    #[tokio::test(start_paused = true)]
    async fn shared_pacing_rejects_admission_when_the_store_fails_under_fail_closed() {
        // Pins: fail_closed turns a pacing-coordination failure into a typed
        // rate-limit error instead of spending an uncoordinated local budget.
        let pacer = shared_pacer(
            PacerConfig::inputs_per_min(120),
            Arc::new(crate::core::coordination_test_support::FailingStore),
            CoordinationFailurePolicy::FailClosed,
        );

        let error = pacer
            .acquire(MODEL, 0, 1)
            .await
            .expect_err("fail_closed must reject pacing it cannot coordinate");
        assert!(matches!(error, MoaError::RateLimited { .. }), "{error}");
    }
}
