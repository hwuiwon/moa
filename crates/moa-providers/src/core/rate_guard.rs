//! Per-provider cooperative rate-limit state shared across provider clones.
//!
//! Two anti-storm mechanisms live here, both process-local and shared across a
//! provider instance's clones (like [`RatePacer`](super::pacer::RatePacer) and
//! [`ConcurrencyLimiter`](super::concurrency::ConcurrencyLimiter)):
//!
//! 1. **429 cooldown** — after a rate-limit response the provider records a
//!    `pause_until` deadline; subsequent calls short-circuit with a typed
//!    [`MoaError::RateLimited`] *without* sleeping, so a caller (or the failover
//!    wrapper) decides whether to wait or fail over rather than every task piling
//!    onto a provider that just said "slow down".
//! 2. **Retry budget** — in-call retries are allowed only while recent retry
//!    volume stays under a fraction of recent request volume, so a burst of
//!    rate-limited calls cannot amplify into a retry storm.
//!
//! Limits are enforced per API key in-process; a multi-instance fleet sharing one
//! key divides the real budget across instances.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moa_core::MoaError;

/// Fallback cooldown applied when a rate-limit response carries no `Retry-After`.
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(5);
/// Sliding window over which request/retry volume is measured for the budget.
const RETRY_BUDGET_WINDOW: Duration = Duration::from_secs(60);
/// Retries may consume up to this percent of the window's request volume.
const RETRY_BUDGET_PERCENT: u64 = 20;
/// Retries always allowed up to this floor, so low-volume callers keep normal
/// retry behavior and the budget only bites under sustained high volume.
const RETRY_BUDGET_FLOOR: u64 = 8;

/// Builds a typed rate-limit error for a provider that is in its 429 cooldown.
pub(crate) fn rate_limited_paused(remaining: Duration) -> MoaError {
    MoaError::RateLimited {
        retries: 0,
        message: format!(
            "provider paused after a recent rate limit; retry after {}ms",
            remaining.as_millis()
        ),
    }
}

/// Builds a typed rate-limit error for a saturated concurrency gate.
pub(crate) fn rate_limited_saturated(waited: Duration) -> MoaError {
    MoaError::RateLimited {
        retries: 0,
        message: format!(
            "provider concurrency gate saturated after waiting {}ms",
            waited.as_millis()
        ),
    }
}

/// Cloneable per-provider rate-limit guard; clones share one set of counters.
#[derive(Clone)]
pub(crate) struct RateGuard {
    inner: Arc<RateGuardInner>,
}

struct RateGuardInner {
    /// Wall-clock base for the millisecond counters below.
    base: Instant,
    /// Cooldown deadline in millis since `base`; `0` means not paused.
    pause_until_ms: AtomicU64,
    /// Start of the current retry-budget window, in millis since `base`.
    window_start_ms: AtomicU64,
    window_requests: AtomicU64,
    window_retries: AtomicU64,
}

impl RateGuard {
    /// Builds an active guard.
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(RateGuardInner {
                base: Instant::now(),
                pause_until_ms: AtomicU64::new(0),
                window_start_ms: AtomicU64::new(0),
                window_requests: AtomicU64::new(0),
                window_retries: AtomicU64::new(0),
            }),
        }
    }

    /// Returns the remaining 429 cooldown, or `None` when not paused.
    pub(crate) fn pause_remaining(&self) -> Option<Duration> {
        self.pause_remaining_at(Instant::now())
    }

    /// Records a rate-limit response, extending the cooldown deadline.
    pub(crate) fn record_rate_limited(&self, retry_after: Option<Duration>) {
        self.record_rate_limited_at(Instant::now(), retry_after);
    }

    /// Counts one outbound request toward the retry-budget window.
    pub(crate) fn note_request(&self) {
        self.note_request_at(Instant::now());
    }

    /// Returns whether another in-call retry is within the budget, consuming one
    /// unit of retry budget when it returns `true`.
    pub(crate) fn allow_retry(&self) -> bool {
        self.allow_retry_at(Instant::now())
    }

    fn pause_remaining_at(&self, now: Instant) -> Option<Duration> {
        let until = self.inner.pause_until_ms.load(Ordering::Relaxed);
        let now_ms = self.ms_since_base(now);
        (until > now_ms).then(|| Duration::from_millis(until - now_ms))
    }

    fn record_rate_limited_at(&self, now: Instant, retry_after: Option<Duration>) {
        let cooldown = retry_after
            .filter(|delay| !delay.is_zero())
            .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN);
        let until = self
            .ms_since_base(now)
            .saturating_add(cooldown.as_millis().min(u128::from(u64::MAX)) as u64);
        // Never shorten an active cooldown set by a concurrent call.
        self.inner
            .pause_until_ms
            .fetch_max(until, Ordering::Relaxed);
    }

    fn note_request_at(&self, now: Instant) {
        self.rotate_window(now);
        self.inner.window_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn allow_retry_at(&self, now: Instant) -> bool {
        self.rotate_window(now);
        let requests = self.inner.window_requests.load(Ordering::Relaxed);
        let retries = self.inner.window_retries.load(Ordering::Relaxed);
        let budget = (requests * RETRY_BUDGET_PERCENT / 100).max(RETRY_BUDGET_FLOOR);
        if retries < budget {
            self.inner.window_retries.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Resets the retry-budget counters when the sliding window has elapsed.
    fn rotate_window(&self, now: Instant) {
        let now_ms = self.ms_since_base(now);
        let start = self.inner.window_start_ms.load(Ordering::Relaxed);
        let window_ms = RETRY_BUDGET_WINDOW.as_millis() as u64;
        if now_ms.saturating_sub(start) < window_ms {
            return;
        }
        // Only the winner of the CAS resets the counters, avoiding a double reset.
        if self
            .inner
            .window_start_ms
            .compare_exchange(start, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.inner.window_requests.store(0, Ordering::Relaxed);
            self.inner.window_retries.store(0, Ordering::Relaxed);
        }
    }

    fn ms_since_base(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.inner.base)
            .as_millis()
            .min(u128::from(u64::MAX)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_short_circuits_until_cooldown_elapses() {
        // Pins: a recorded 429 pauses the provider for the cooldown window and the
        // pause clears once the window elapses, without any sleeping.
        let guard = RateGuard::new();
        let now = Instant::now();
        assert!(guard.pause_remaining_at(now).is_none());

        guard.record_rate_limited_at(now, Some(Duration::from_secs(10)));
        assert!(
            guard.pause_remaining_at(now).is_some(),
            "provider should be paused immediately after a 429"
        );
        assert!(
            guard
                .pause_remaining_at(now + Duration::from_secs(11))
                .is_none(),
            "pause should clear once the cooldown elapses"
        );
    }

    #[test]
    fn missing_retry_after_uses_default_cooldown() {
        // Pins: a 429 with no Retry-After still pauses for the default cooldown.
        let guard = RateGuard::new();
        let now = Instant::now();
        guard.record_rate_limited_at(now, None);
        assert!(guard.pause_remaining_at(now).is_some());
        assert!(
            guard
                .pause_remaining_at(now + DEFAULT_RATE_LIMIT_COOLDOWN + Duration::from_millis(1))
                .is_none()
        );
    }

    #[test]
    fn retry_budget_allows_the_floor_then_fails_fast() {
        // Pins: retries are allowed up to the floor under low volume, then the
        // budget blocks further retries until request volume grows.
        let guard = RateGuard::new();
        let now = Instant::now();
        guard.note_request_at(now);
        for _ in 0..RETRY_BUDGET_FLOOR {
            assert!(
                guard.allow_retry_at(now),
                "retries within the floor are allowed"
            );
        }
        assert!(
            !guard.allow_retry_at(now),
            "the retry budget must fail fast once the floor is exhausted at low volume"
        );
    }

    #[test]
    fn retry_budget_scales_to_twenty_percent_of_request_volume() {
        // Pins: under high request volume the budget grows to ~20% of requests.
        let guard = RateGuard::new();
        let now = Instant::now();
        for _ in 0..1_000 {
            guard.note_request_at(now);
        }
        let mut allowed = 0;
        for _ in 0..1_000 {
            if guard.allow_retry_at(now) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 200, "retry budget should be 20% of 1000 requests");
    }
}
