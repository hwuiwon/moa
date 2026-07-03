//! In-process request/input pacing for provider API rate limits.
//!
//! Providers such as Cohere document per-minute ceilings in different units:
//! embeddings are limited by *inputs* per minute while rerank/chat are limited
//! by *requests* per minute. [`RatePacer`] applies a token bucket to each
//! configured dimension so a busy in-process caller stays under those ceilings
//! before the HTTP request is sent, complementing (not replacing) any
//! concurrency window a provider already applies.
//!
//! Pacing is process-local with no distributed coordination. Provider rate
//! limits are enforced per API key, so a multi-instance fleet sharing one key
//! should configure each instance with its fraction of the documented budget.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::time::{Instant, sleep};

const SECONDS_PER_MINUTE: f64 = 60.0;

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
}

/// A cloneable per-endpoint pacer; clones share one set of token buckets.
#[derive(Clone)]
pub(crate) struct RatePacer {
    inner: Arc<PacerInner>,
}

struct PacerInner {
    state: Mutex<PacerState>,
}

struct PacerState {
    requests: Option<Bucket>,
    inputs: Option<Bucket>,
}

impl RatePacer {
    /// Builds a pacer from a per-minute limit configuration.
    pub(crate) fn new(config: PacerConfig) -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(PacerInner {
                state: Mutex::new(PacerState {
                    requests: config
                        .max_requests_per_min
                        .map(|limit| Bucket::new(limit, now)),
                    inputs: config
                        .max_inputs_per_min
                        .map(|limit| Bucket::new(limit, now)),
                }),
            }),
        }
    }

    /// Waits until both configured dimensions can admit `requests` and `inputs`,
    /// then consumes that budget.
    ///
    /// Unbounded dimensions are ignored, so a fully-disabled pacer returns
    /// immediately without touching the clock.
    pub(crate) async fn acquire(&self, requests: u32, inputs: u32) {
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
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pacer_paces_inputs_per_minute_after_the_bucket_drains() {
        // Pins: once the input bucket is drained, further inputs wait for refill
        // at limit/60 per second before proceeding.
        let pacer = RatePacer::new(PacerConfig::inputs_per_min(120));
        // Drain the full minute of capacity up front (initial burst is allowed).
        pacer.acquire(0, 120).await;

        let waiter = pacer.clone();
        let task = tokio::spawn(async move { waiter.acquire(0, 2).await });
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "a drained input bucket must make the next inputs wait for refill"
        );

        // 120/min = 2 tokens/sec, so 2 inputs need one second of refill.
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        task.await.expect("acquire should complete after refill");
    }

    #[tokio::test(start_paused = true)]
    async fn pacer_enforces_the_stricter_of_two_dimensions() {
        // Pins: both request and input dimensions must be satisfied; the slower
        // one governs. Here inputs refill slower than requests.
        let pacer = RatePacer::new(PacerConfig {
            max_requests_per_min: Some(600), // 10/sec
            max_inputs_per_min: Some(60),    // 1/sec
        });
        pacer.acquire(600, 60).await; // drain both

        let waiter = pacer.clone();
        // 1 request refills in 0.1s, but 1 input needs 1s; the input bound wins.
        let task = tokio::spawn(async move { waiter.acquire(1, 1).await });
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "the input dimension should still be gating at T+0.5s"
        );
        tokio::time::advance(Duration::from_millis(500)).await;
        tokio::task::yield_now().await;
        task.await
            .expect("acquire should complete once inputs refill");
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_pacer_never_blocks_even_for_large_demand() {
        // Pins: an unbounded pacer is a no-op regardless of the requested budget.
        let pacer = RatePacer::new(PacerConfig::disabled());
        pacer.acquire(1_000_000, 1_000_000).await;
        pacer.acquire(1_000_000, 1_000_000).await;
    }

    #[tokio::test(start_paused = true)]
    async fn demand_larger_than_capacity_drains_without_deadlock() {
        // Pins: a single request bigger than the whole per-minute budget still
        // makes progress instead of waiting forever.
        let pacer = RatePacer::new(PacerConfig::inputs_per_min(100));
        pacer.acquire(0, 250).await;
    }
}
