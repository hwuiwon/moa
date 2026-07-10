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

/// Default in-flight ceiling for embedding providers.
///
/// Embedding calls fan out over document batches, so a small default keeps one
/// busy ingestion run from opening an unbounded number of sockets to the provider
/// while still overlapping enough round trips to stay throughput-bound.
pub(crate) const DEFAULT_EMBEDDING_CONCURRENCY: usize = 8;

/// Default in-flight ceiling for rerank providers.
pub(crate) const DEFAULT_RERANK_CONCURRENCY: usize = 8;

/// Default in-flight ceiling for chat/LLM providers, per API key.
///
/// A generous bound that still prevents one process from opening an unbounded
/// number of simultaneous connections to a provider (exhausting sockets or the
/// key's concurrency window) when many turns fan out at once. Operators can raise
/// or lower it per key via `max_concurrent_requests`, and an explicit `0` opts
/// back into unbounded.
pub(crate) const DEFAULT_LLM_CONCURRENCY: usize = 32;

/// How long a call waits for a concurrency slot before reporting "blocked".
///
/// A wait beyond this is treated as a failover-eligible block rather than an
/// unbounded queue, so a saturated primary hands off to a fallback quickly.
pub(crate) const DEFAULT_BLOCK_THRESHOLD: Duration = Duration::from_secs(2);

/// An acquired in-flight slot held for the lifetime of one outbound call.
///
/// `Unbounded` carries no permit (the limiter imposes no ceiling); `Held` owns a
/// semaphore permit that frees its slot on drop. Both keep the slot reserved for
/// as long as the lease is bound, matching [`ConcurrencyLimiter::acquire`].
pub(crate) enum PermitLease {
    /// The limiter is unbounded; there is no slot to release.
    Unbounded,
    /// A held semaphore permit; the slot is reserved until this is dropped. The
    /// permit is never read — it exists purely for its `Drop` side effect.
    Held(#[allow(dead_code)] OwnedSemaphorePermit),
}

/// A cloneable in-flight concurrency gate for one provider instance.
///
/// A `None` inner semaphore means unbounded: [`ConcurrencyLimiter::acquire`]
/// returns immediately without allocating a permit.
#[derive(Clone)]
pub(crate) struct ConcurrencyLimiter {
    inner: Option<Arc<Semaphore>>,
}

impl ConcurrencyLimiter {
    /// Builds a limiter that admits at most `max_in_flight` concurrent requests.
    ///
    /// A `max_in_flight` of zero is treated as unbounded rather than a permanently
    /// closed gate, matching the "unset means no limit" configuration semantics.
    pub(crate) fn new(max_in_flight: usize) -> Self {
        Self {
            inner: (max_in_flight > 0).then(|| Arc::new(Semaphore::new(max_in_flight))),
        }
    }

    /// Returns whether this limiter imposes a finite in-flight ceiling.
    #[cfg(test)]
    pub(crate) fn is_bounded(&self) -> bool {
        self.inner.is_some()
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
        let Some(semaphore) = &self.inner else {
            return Some(PermitLease::Unbounded);
        };
        match tokio::time::timeout(max_wait, Arc::clone(semaphore).acquire_owned()).await {
            // Acquired within the deadline.
            Ok(Ok(permit)) => Some(PermitLease::Held(permit)),
            // Semaphore closed (never in practice); degrade to unbounded.
            Ok(Err(_)) => Some(PermitLease::Unbounded),
            // Still saturated after `max_wait`: report blocked.
            Err(_) => None,
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
