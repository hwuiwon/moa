//! Fail-open circuit breaker for query rewriting.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Sliding-window circuit breaker for fail-open query rewriting.
pub struct CircuitBreaker {
    failures: AtomicU32,
    successes: AtomicU32,
    last_reset: AtomicU64,
    tripped_until: AtomicU64,
    threshold: f64,
    window_secs: u64,
    cooldown_secs: u64,
}

impl CircuitBreaker {
    /// Creates a circuit breaker with an error-rate threshold, window, and cooldown.
    #[must_use]
    pub fn new(threshold: f64, window_secs: u64, cooldown_secs: u64) -> Self {
        Self {
            failures: AtomicU32::new(0),
            successes: AtomicU32::new(0),
            last_reset: AtomicU64::new(now_epoch_millis()),
            tripped_until: AtomicU64::new(0),
            threshold,
            window_secs,
            cooldown_secs,
        }
    }

    /// Returns whether the circuit is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let now = now_epoch_millis();
        let tripped_until = self.tripped_until.load(Ordering::Relaxed);
        if tripped_until > now {
            return true;
        }

        if tripped_until != 0 {
            self.tripped_until.store(0, Ordering::Relaxed);
            self.reset_window(now);
        } else {
            self.rotate_window_if_needed(now);
        }

        false
    }

    /// Records a successful rewriter call.
    pub fn record_success(&self) {
        self.rotate_window_if_needed(now_epoch_millis());
        self.successes.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a failed rewriter call and trips the circuit when the error rate is too high.
    pub fn record_failure(&self) {
        let now = now_epoch_millis();
        self.rotate_window_if_needed(now);
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        let successes = self.successes.load(Ordering::Relaxed);
        let total = failures + successes;
        if total == 0 {
            return;
        }

        let failure_rate = f64::from(failures) / f64::from(total);
        if failure_rate > self.threshold {
            let cooldown_ms = self.cooldown_secs.saturating_mul(1_000);
            self.tripped_until
                .store(now.saturating_add(cooldown_ms), Ordering::Relaxed);
        }
    }

    fn rotate_window_if_needed(&self, now: u64) {
        let window_ms = self.window_secs.saturating_mul(1_000);
        if window_ms == 0 {
            self.reset_window(now);
            return;
        }

        let last_reset = self.last_reset.load(Ordering::Relaxed);
        if now.saturating_sub(last_reset) < window_ms {
            return;
        }

        self.reset_window(now);
    }

    fn reset_window(&self, now: u64) {
        self.failures.store(0, Ordering::Relaxed);
        self.successes.store(0, Ordering::Relaxed);
        self.last_reset.store(now, Ordering::Relaxed);
    }
}

fn now_epoch_millis() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
