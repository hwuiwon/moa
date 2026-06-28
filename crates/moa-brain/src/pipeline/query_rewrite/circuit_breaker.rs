//! Fail-open circuit breaker for query rewriting.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Injectable millisecond clock. Defaults to the real wall clock in production
/// and is overridden in tests so window rotation and cooldown are deterministic.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Sliding-window circuit breaker for fail-open query rewriting.
pub struct CircuitBreaker {
    failures: AtomicU32,
    successes: AtomicU32,
    last_reset: AtomicU64,
    tripped_until: AtomicU64,
    threshold: f64,
    window_secs: u64,
    cooldown_secs: u64,
    clock: Clock,
}

impl CircuitBreaker {
    /// Creates a circuit breaker with an error-rate threshold, window, and cooldown.
    #[must_use]
    pub fn new(threshold: f64, window_secs: u64, cooldown_secs: u64) -> Self {
        Self::with_clock(
            threshold,
            window_secs,
            cooldown_secs,
            Arc::new(now_epoch_millis),
        )
    }

    /// Creates a circuit breaker driven by an injected millisecond clock.
    ///
    /// Production code uses [`CircuitBreaker::new`], which supplies the wall
    /// clock; tests inject a controllable clock to exercise window rotation and
    /// cooldown without sleeping.
    #[must_use]
    pub(crate) fn with_clock(
        threshold: f64,
        window_secs: u64,
        cooldown_secs: u64,
        clock: Clock,
    ) -> Self {
        let last_reset = clock();
        Self {
            failures: AtomicU32::new(0),
            successes: AtomicU32::new(0),
            last_reset: AtomicU64::new(last_reset),
            tripped_until: AtomicU64::new(0),
            threshold,
            window_secs,
            cooldown_secs,
            clock,
        }
    }

    /// Returns whether the circuit is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let now = self.now();
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
        self.rotate_window_if_needed(self.now());
        self.successes.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a failed rewriter call and trips the circuit when the error rate is too high.
    pub fn record_failure(&self) {
        let now = self.now();
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

    fn now(&self) -> u64 {
        (self.clock)()
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::CircuitBreaker;

    /// Builds a breaker driven by a shared, manually advanced millisecond clock.
    fn breaker_with_clock(
        threshold: f64,
        window_secs: u64,
        cooldown_secs: u64,
        clock: &Arc<AtomicU64>,
    ) -> CircuitBreaker {
        let clock = clock.clone();
        CircuitBreaker::with_clock(
            threshold,
            window_secs,
            cooldown_secs,
            Arc::new(move || clock.load(Ordering::Relaxed)),
        )
    }

    fn advance(clock: &Arc<AtomicU64>, millis: u64) {
        clock.fetch_add(millis, Ordering::Relaxed);
    }

    #[test]
    fn window_rotation_forgets_prior_outcomes() {
        // Pins: once the sliding window elapses the prior success/failure counts
        // are dropped, so a fresh failure is judged against an empty window.
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let breaker = breaker_with_clock(0.5, 60, 60, &clock);

        // Healthy window: 4 successes and 1 failure -> rate 0.2, well under 0.5.
        for _ in 0..4 {
            breaker.record_success();
        }
        breaker.record_failure();
        assert!(
            !breaker.is_open(),
            "20% failure rate must not trip a 50% threshold"
        );

        // Past the window: the next failure is evaluated against a reset window.
        // If the prior successes survived the rate would be 2/6 (0.33) and the
        // breaker would stay closed; tripping proves the window was cleared.
        advance(&clock, 61_000);
        breaker.record_failure();
        assert!(
            breaker.is_open(),
            "post-rotation failure must trip from a cleared window"
        );
    }

    #[test]
    fn fail_rate_threshold_uses_strict_greater_than_boundary() {
        // Pins: a failure rate exactly equal to the threshold does NOT trip; only
        // a rate strictly above it does. Guards the `>` vs `>=` comparison.
        let clock = Arc::new(AtomicU64::new(5_000_000));
        let breaker = breaker_with_clock(0.5, 60, 60, &clock);

        breaker.record_success();
        breaker.record_failure(); // 1/2 == 0.5, equal to threshold -> stay closed
        assert!(
            !breaker.is_open(),
            "failure rate equal to threshold must not trip"
        );

        breaker.record_failure(); // 2/3 == 0.66, strictly above threshold -> trip
        assert!(breaker.is_open(), "failure rate above threshold must trip");
    }

    #[test]
    fn fail_open_resets_after_cooldown_elapses() {
        // Pins: a tripped breaker stays open for the whole cooldown, then fails
        // open (closes) once the cooldown elapses, and is healthy afterwards.
        let clock = Arc::new(AtomicU64::new(2_000_000));
        let breaker = breaker_with_clock(0.05, 60, 30, &clock);

        breaker.record_failure(); // 1/1 == 1.0 > 0.05 -> trips for 30s
        assert!(breaker.is_open(), "first failure should trip the breaker");

        advance(&clock, 29_000);
        assert!(
            breaker.is_open(),
            "breaker must stay open during the cooldown"
        );

        advance(&clock, 2_000); // 31s total, past the 30s cooldown
        assert!(
            !breaker.is_open(),
            "breaker must fail open once the cooldown elapses"
        );

        // The window was cleared on reset, so a single new failure re-trips.
        breaker.record_failure();
        assert!(
            breaker.is_open(),
            "breaker should re-trip from a clean window after cooldown"
        );
    }
}
