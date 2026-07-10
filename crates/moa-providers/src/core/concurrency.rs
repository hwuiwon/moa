//! In-process per-provider concurrency limiting for outbound API calls.
//!
//! A provider's per-minute [`RatePacer`](super::pacer::RatePacer) bounds how fast
//! requests may be *started*, but it does not bound how many are *in flight* at
//! once. A burst of concurrent callers can therefore open many simultaneous
//! connections to one provider, exhausting sockets or the provider's per-key
//! concurrency window. [`ConcurrencyLimiter`] adds a fixed-size in-flight gate in
//! front of each provider instance.
//!
//! # Ordering relative to the pacer
//!
//! Callers acquire the concurrency permit *before* pacing and hold it across the
//! outbound HTTP call:
//!
//! ```ignore
//! let _permit = self.limiter.acquire().await; // 1. take an in-flight slot
//! self.pacer.acquire(1, inputs).await;         // 2. then spend rate budget
//! // 3. run the request while both the permit and budget are held
//! ```
//!
//! Taking the permit first means a request queued behind a full gate consumes no
//! per-minute rate budget until it actually holds a slot, so a burst cannot spend
//! the minute's allowance on requests that are still waiting to run.
//!
//! Like the pacer, a limiter is process-local and its clones share one semaphore,
//! so every client cloned from one provider instance enforces a single shared
//! in-flight budget.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::global_concurrency::{GlobalConcurrency, GlobalLeaseGuard};

/// Default in-flight ceiling used only by direct provider construction (`new`,
/// `from_env`, tests) — one flat number per provider, not per call kind.
///
/// Provider rate limits are a per-credential account-tier property, so a
/// credential serving several call kinds shares one budget; there is no per-kind
/// default. This mirrors the workspace fallback
/// `ProviderConcurrencyConfig::default_max_in_flight`. The config-driven
/// `from_config` path builds each provider's limiter per (provider, credential)
/// and overrides this constant, so it applies only to providers built outside
/// that path.
pub(crate) const DEFAULT_MAX_IN_FLIGHT: usize = 16;

/// How long a call waits for a concurrency slot before reporting "blocked".
///
/// A wait beyond this is treated as a failover-eligible block rather than an
/// unbounded queue, so a saturated primary hands off to a fallback quickly.
pub(crate) const DEFAULT_BLOCK_THRESHOLD: Duration = Duration::from_secs(2);

/// An acquired in-flight slot held for the lifetime of one outbound call.
///
/// `Unbounded` carries no permit (the limiter imposes no ceiling); `Held` owns a
/// semaphore permit that frees its slot on drop; `Global` owns a runtime-store
/// lease that is released on drop (see [`GlobalLeaseGuard`]). All keep the slot
/// reserved for as long as the lease is bound, matching
/// [`ConcurrencyLimiter::acquire`].
pub(crate) enum PermitLease {
    /// The limiter is unbounded; there is no slot to release.
    Unbounded,
    /// A held semaphore permit; the slot is reserved until this is dropped. The
    /// permit is never read — it exists purely for its `Drop` side effect.
    Held(#[allow(dead_code)] OwnedSemaphorePermit),
    /// A held cross-replica lease; releasing it deletes the lease on drop.
    Global(#[allow(dead_code)] GlobalLeaseGuard),
}

/// A cloneable in-flight concurrency gate for one provider instance.
///
/// A local limiter with no ceiling is unbounded ([`ConcurrencyLimiter::acquire`]
/// returns immediately without allocating a permit). A global limiter coordinates
/// its ceiling across replicas through the runtime store. Every limiter carries a
/// `block_threshold`: the wait [`acquire`](Self::acquire) allows before reporting
/// a failover-eligible saturated signal.
#[derive(Clone)]
pub(crate) struct ConcurrencyLimiter {
    mode: LimiterMode,
    block_threshold: Duration,
}

#[derive(Clone)]
enum LimiterMode {
    /// Process-local gate; `None` is unbounded.
    Local(Option<Arc<Semaphore>>),
    /// Cross-replica gate coordinated through the runtime store.
    Global(Arc<GlobalConcurrency>),
}

impl ConcurrencyLimiter {
    /// Builds a process-local limiter admitting at most `max_in_flight` requests,
    /// with the default block threshold.
    ///
    /// A `max_in_flight` of zero is treated as unbounded rather than a permanently
    /// closed gate, matching the "unset means no limit" configuration semantics.
    pub(crate) fn new(max_in_flight: usize) -> Self {
        Self::local(max_in_flight, DEFAULT_BLOCK_THRESHOLD)
    }

    /// Builds a process-local limiter with an explicit block threshold.
    pub(crate) fn local(max_in_flight: usize, block_threshold: Duration) -> Self {
        Self {
            mode: LimiterMode::Local(
                (max_in_flight > 0).then(|| Arc::new(Semaphore::new(max_in_flight))),
            ),
            block_threshold,
        }
    }

    /// Builds a process-local limiter over a caller-provided (possibly shared)
    /// semaphore. `None` is unbounded; passing one shared semaphore lets multiple
    /// limiters contend for a single budget.
    pub(crate) fn from_local_semaphore(
        semaphore: Option<Arc<Semaphore>>,
        block_threshold: Duration,
    ) -> Self {
        Self {
            mode: LimiterMode::Local(semaphore),
            block_threshold,
        }
    }

    /// Builds a limiter that coordinates its ceiling across replicas.
    pub(crate) fn global(limiter: GlobalConcurrency, block_threshold: Duration) -> Self {
        Self {
            mode: LimiterMode::Global(Arc::new(limiter)),
            block_threshold,
        }
    }

    /// Returns whether this limiter imposes a finite in-flight ceiling.
    #[cfg(test)]
    pub(crate) fn is_bounded(&self) -> bool {
        match &self.mode {
            LimiterMode::Local(inner) => inner.is_some(),
            LimiterMode::Global(_) => true,
        }
    }

    /// Takes an in-flight slot, waiting up to this limiter's configured block
    /// threshold. Returns `None` when the gate stays saturated for the whole wait.
    pub(crate) async fn acquire(&self) -> Option<PermitLease> {
        self.acquire_within(self.block_threshold).await
    }

    /// Returns the wait this limiter allows before reporting a saturated gate.
    pub(crate) fn block_threshold(&self) -> Duration {
        self.block_threshold
    }

    /// Takes an in-flight slot, waiting at most `max_wait` for one to free up.
    ///
    /// A `max_wait` of zero makes this a non-blocking try-acquire (returns `None`
    /// immediately when the gate is saturated).
    ///
    /// Returns `None` when the gate stays saturated for longer than `max_wait`,
    /// which callers treat as a blocked signal eligible for failover instead of
    /// queueing indefinitely.
    pub(crate) async fn acquire_within(&self, max_wait: Duration) -> Option<PermitLease> {
        match &self.mode {
            LimiterMode::Local(None) => Some(PermitLease::Unbounded),
            LimiterMode::Local(Some(semaphore)) => {
                match tokio::time::timeout(max_wait, Arc::clone(semaphore).acquire_owned()).await {
                    // Acquired within the deadline.
                    Ok(Ok(permit)) => Some(PermitLease::Held(permit)),
                    // Semaphore closed (never in practice); degrade to unbounded.
                    Ok(Err(_)) => Some(PermitLease::Unbounded),
                    // Still saturated after `max_wait`: report blocked.
                    Err(_) => None,
                }
            }
            LimiterMode::Global(limiter) => limiter.acquire(max_wait).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::ConcurrencyLimiter;

    #[tokio::test(start_paused = true)]
    async fn limiter_caps_concurrent_holders_at_the_configured_bound() {
        // Pins: a limiter of N never lets more than N permits be held at once, even
        // when far more tasks contend for a slot simultaneously.
        const BOUND: usize = 2;
        const TASKS: usize = 8;

        let limiter = ConcurrencyLimiter::new(BOUND);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let limiter = limiter.clone();
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let _permit = limiter
                    .acquire_within(Duration::from_secs(3600))
                    .await
                    .expect("a slot frees within the wait");
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Force overlap: hold the slot across an await so contending tasks
                // must queue on the semaphore rather than running end-to-end first.
                tokio::time::sleep(Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }

        for handle in handles {
            handle.await.expect("limiter task should not panic");
        }

        assert_eq!(max_seen.load(Ordering::SeqCst), BOUND);
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_within_reports_blocked_when_the_gate_stays_saturated() {
        // Pins: a saturated gate returns None (a failover-eligible block) once the
        // wait elapses, while an unbounded gate always yields a lease.
        use std::time::Duration;

        let limiter = super::ConcurrencyLimiter::new(1);
        let held = limiter
            .acquire_within(Duration::ZERO)
            .await
            .expect("the first slot is free");
        assert!(
            limiter
                .acquire_within(Duration::from_millis(50))
                .await
                .is_none(),
            "a second caller must report blocked while the only slot is held"
        );
        drop(held);
        assert!(
            limiter.acquire_within(Duration::ZERO).await.is_some(),
            "the slot frees for the next caller once the lease is dropped"
        );
        assert!(
            super::ConcurrencyLimiter::new(0)
                .acquire_within(Duration::ZERO)
                .await
                .is_some(),
            "an unbounded gate never blocks"
        );
    }

    #[tokio::test]
    async fn zero_bound_limiter_is_unbounded_and_never_gates() {
        // Pins: a zero bound degrades to an unbounded gate (the explicit opt-out),
        // so acquire_within always yields a lease without blocking.
        let limiter = ConcurrencyLimiter::new(0);
        assert!(
            limiter.acquire_within(Duration::ZERO).await.is_some(),
            "a zero-bound (unbounded) gate never blocks"
        );
    }
}
