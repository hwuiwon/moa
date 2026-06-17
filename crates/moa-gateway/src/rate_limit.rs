//! Rate-limit retry and per-channel send pacing for gateway adapters.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{MoaError, Platform, Result};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};

/// Normalized response metadata needed by the gateway rate-limit policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewaySendResponse {
    /// HTTP status returned by the platform API.
    pub status: u16,
    /// Header values used by retry policies.
    pub headers: Vec<(String, String)>,
    /// Response body, used for typed error messages.
    pub body: String,
}

impl GatewaySendResponse {
    /// Creates a normalized response with no headers.
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    /// Adds one response header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Returns true when this response represents a platform rate-limit response.
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
    }

    fn retry_after_for_platform(&self, _platform: Platform) -> Duration {
        let retry_after = self.header("retry-after");
        retry_after
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(Duration::from_secs_f64)
            .unwrap_or_else(|| Duration::from_secs(1))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// In-memory counters emitted by the gateway rate-limit policy.
#[derive(Debug, Default)]
pub struct GatewayRateLimitMetrics {
    counters: Mutex<HashMap<String, u64>>,
}

impl GatewayRateLimitMetrics {
    /// Increments one named counter with platform and outcome dimensions encoded in the key.
    pub async fn increment(&self, name: &str, platform: Platform, outcome: Option<&str>) {
        let key = match outcome {
            Some(outcome) => format!("{name}|platform={platform}|outcome={outcome}"),
            None => format!("{name}|platform={platform}"),
        };
        let mut counters = self.counters.lock().await;
        *counters.entry(key).or_insert(0) += 1;
    }

    /// Returns one counter value.
    pub async fn counter(&self, name: &str, platform: Platform, outcome: Option<&str>) -> u64 {
        let key = match outcome {
            Some(outcome) => format!("{name}|platform={platform}|outcome={outcome}"),
            None => format!("{name}|platform={platform}"),
        };
        self.counters.lock().await.get(&key).copied().unwrap_or(0)
    }
}

/// Rate-limit retry policy and per-channel pacing state.
#[derive(Debug, Clone)]
pub struct GatewayRateLimiter {
    platform: Platform,
    max_retries: usize,
    per_channel_interval: Duration,
    delay_first_send: bool,
    next_send_at: Arc<Mutex<HashMap<String, Instant>>>,
    metrics: Arc<GatewayRateLimitMetrics>,
}

impl GatewayRateLimiter {
    /// Creates a rate limiter with conservative defaults for a platform.
    pub fn for_platform(platform: Platform) -> Self {
        let per_channel_interval = match platform {
            Platform::Slack => Duration::from_secs(1),
            _ => Duration::ZERO,
        };
        Self {
            platform,
            max_retries: 3,
            per_channel_interval,
            delay_first_send: true,
            next_send_at: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(GatewayRateLimitMetrics::default()),
        }
    }

    /// Overrides the maximum number of retries after the initial attempt.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Overrides the per-channel minimum interval.
    #[must_use]
    pub fn with_per_channel_interval(mut self, interval: Duration) -> Self {
        self.per_channel_interval = interval;
        self
    }

    /// Controls whether the first send in a channel is delayed by one interval.
    #[must_use]
    pub fn with_delay_first_send(mut self, delay_first_send: bool) -> Self {
        self.delay_first_send = delay_first_send;
        self
    }

    /// Returns the shared metrics registry for this limiter.
    pub fn metrics(&self) -> Arc<GatewayRateLimitMetrics> {
        self.metrics.clone()
    }

    /// Runs one platform send operation with retry/backoff and channel pacing.
    pub async fn send_with_retry<F, Fut>(
        &self,
        channel_key: &str,
        mut operation: F,
    ) -> Result<GatewaySendResponse>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<GatewaySendResponse>>,
    {
        let mut retries = 0;
        loop {
            self.wait_for_channel_slot(channel_key).await;
            let response = operation().await?;
            if !response.is_rate_limited() {
                if retries > 0 {
                    self.metrics
                        .increment(
                            "gateway_send_retries_total",
                            self.platform.clone(),
                            Some("success"),
                        )
                        .await;
                }
                return Ok(response);
            }

            self.metrics
                .increment(
                    "gateway_send_429_received_total",
                    self.platform.clone(),
                    None,
                )
                .await;

            if retries >= self.max_retries {
                self.metrics
                    .increment(
                        "gateway_send_retries_total",
                        self.platform.clone(),
                        Some("exhausted"),
                    )
                    .await;
                return Err(MoaError::RateLimited {
                    retries,
                    message: format!(
                        "{} send remained rate limited after {retries} retries: {}",
                        self.platform, response.body
                    ),
                });
            }

            retries += 1;
            sleep(response.retry_after_for_platform(self.platform.clone())).await;
        }
    }

    async fn wait_for_channel_slot(&self, channel_key: &str) {
        if self.per_channel_interval.is_zero() {
            return;
        }

        let now = Instant::now();
        let wait_until = {
            let mut next_send_at = self.next_send_at.lock().await;
            let entry = next_send_at
                .entry(channel_key.to_string())
                .or_insert_with(|| {
                    if self.delay_first_send {
                        now + self.per_channel_interval
                    } else {
                        now
                    }
                });
            let wait_until = *entry;
            *entry = wait_until + self.per_channel_interval;
            wait_until
        };

        if wait_until > now {
            sleep(wait_until - now).await;
        }
    }
}
