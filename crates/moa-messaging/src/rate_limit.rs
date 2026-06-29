//! Rate-limit retry and per-channel send pacing for messaging adapters.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moa_core::traits::RuntimeCacheStore;
use moa_core::{Channel, MoaError, Result};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};

const RATE_LIMIT_CACHE_TTL: Duration = Duration::from_secs(300);

/// Normalized response metadata needed by the messaging rate-limit policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingSendResponse {
    /// HTTP status returned by the channel provider API.
    pub status: u16,
    /// Header values used by retry policies.
    pub headers: Vec<(String, String)>,
    /// Response body, used for typed error messages.
    pub body: String,
}

impl MessagingSendResponse {
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

    /// Returns true when this response represents a channel-provider rate-limit response.
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
    }

    /// Returns normalized failure metadata for non-success channel-provider responses.
    pub fn failure_for_channel(&self, channel: Channel) -> Option<MessagingSendFailure> {
        if self.status < 400 {
            return None;
        }

        let retry_after = self.retry_after_opt();
        let class = if is_retryable_status(self.status) {
            MessagingFailureClass::Retryable
        } else {
            MessagingFailureClass::Permanent
        };
        Some(MessagingSendFailure {
            status: self.status,
            class,
            retry_after,
            reason: format!("{channel} send returned HTTP status {}", self.status),
        })
    }

    fn retry_after(&self) -> Duration {
        self.retry_after_opt()
            .unwrap_or_else(|| Duration::from_secs(1))
    }

    fn retry_after_opt(&self) -> Option<Duration> {
        let retry_after = self.header("retry-after");
        retry_after
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .map(Duration::from_secs_f64)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Retry classification for a messaging send response failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagingFailureClass {
    /// A later send may succeed after the referenced transient condition clears.
    Retryable,
    /// A retry is not expected to help without configuration, request, or recipient changes.
    Permanent,
}

impl MessagingFailureClass {
    /// Returns the stable telemetry label for this failure class.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Retryable => "retryable",
            Self::Permanent => "permanent",
        }
    }
}

/// Structured metadata for a failed messaging send response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingSendFailure {
    /// HTTP status returned by the channel provider API.
    pub status: u16,
    /// Retry classification for this response.
    pub class: MessagingFailureClass,
    /// Parsed `Retry-After` hint when the provider supplied one.
    pub retry_after: Option<Duration>,
    /// Human-readable reason safe for logs and operator UI.
    pub reason: String,
}

impl MessagingSendFailure {
    /// Returns whether this response can be retried by a durable caller.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class == MessagingFailureClass::Retryable
    }
}

/// In-memory counters emitted by the messaging rate-limit policy.
#[derive(Debug, Default)]
pub struct MessagingRateLimitMetrics {
    counters: Mutex<HashMap<String, u64>>,
}

impl MessagingRateLimitMetrics {
    /// Increments one named counter with channel and outcome dimensions encoded in the key.
    pub async fn increment(&self, name: &str, channel: Channel, outcome: Option<&str>) {
        let key = match outcome {
            Some(outcome) => format!("{name}|channel={channel}|outcome={outcome}"),
            None => format!("{name}|channel={channel}"),
        };
        let mut counters = self.counters.lock().await;
        *counters.entry(key).or_insert(0) += 1;
    }

    /// Returns one counter value.
    pub async fn counter(&self, name: &str, channel: Channel, outcome: Option<&str>) -> u64 {
        let key = match outcome {
            Some(outcome) => format!("{name}|channel={channel}|outcome={outcome}"),
            None => format!("{name}|channel={channel}"),
        };
        self.counters.lock().await.get(&key).copied().unwrap_or(0)
    }
}

/// Rate-limit retry policy and per-channel pacing state.
#[derive(Clone)]
pub struct MessagingRateLimiter {
    channel: Channel,
    max_retries: usize,
    per_channel_interval: Duration,
    delay_first_send: bool,
    runtime_cache: Option<Arc<dyn RuntimeCacheStore>>,
    next_send_at: Arc<Mutex<HashMap<String, Instant>>>,
    metrics: Arc<MessagingRateLimitMetrics>,
}

impl fmt::Debug for MessagingRateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagingRateLimiter")
            .field("channel", &self.channel)
            .field("max_retries", &self.max_retries)
            .field("per_channel_interval", &self.per_channel_interval)
            .field("delay_first_send", &self.delay_first_send)
            .field("runtime_cache_configured", &self.runtime_cache.is_some())
            .finish_non_exhaustive()
    }
}

impl MessagingRateLimiter {
    /// Creates a rate limiter with conservative defaults for a channel.
    pub fn for_channel(channel: Channel) -> Self {
        let per_channel_interval = match channel {
            Channel::Slack => Duration::from_secs(1),
            _ => Duration::ZERO,
        };
        Self {
            channel,
            max_retries: 3,
            per_channel_interval,
            delay_first_send: true,
            runtime_cache: None,
            next_send_at: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(MessagingRateLimitMetrics::default()),
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

    /// Uses a shared runtime cache for cross-replica channel pacing.
    #[must_use]
    pub fn with_runtime_cache(mut self, runtime_cache: Arc<dyn RuntimeCacheStore>) -> Self {
        self.runtime_cache = Some(runtime_cache);
        self
    }

    /// Returns the shared metrics registry for this limiter.
    pub fn metrics(&self) -> Arc<MessagingRateLimitMetrics> {
        self.metrics.clone()
    }

    /// Runs one channel-provider send operation with retry/backoff and channel pacing.
    pub async fn send_with_retry<F, Fut>(
        &self,
        channel_key: &str,
        mut operation: F,
    ) -> Result<MessagingSendResponse>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<MessagingSendResponse>>,
    {
        let mut retries = 0;
        loop {
            self.wait_for_channel_slot(channel_key).await?;
            let response = operation().await?;
            if !response.is_rate_limited() {
                if let Some(failure) = response.failure_for_channel(self.channel) {
                    self.metrics
                        .increment(
                            "messaging_send_failures_total",
                            self.channel,
                            Some(failure.class.label()),
                        )
                        .await;
                    tracing::warn!(
                        messaging.channel = %self.channel,
                        http.status_code = failure.status,
                        retryable = failure.is_retryable(),
                        failure_class = failure.class.label(),
                        error = %failure.reason,
                        "messaging channel send returned a non-success status"
                    );
                    return Err(MoaError::HttpStatus {
                        status: failure.status,
                        retry_after: failure.retry_after,
                        message: response.body,
                    });
                }
                if retries > 0 {
                    self.metrics
                        .increment(
                            "messaging_send_retries_total",
                            self.channel,
                            Some("success"),
                        )
                        .await;
                }
                return Ok(response);
            }

            self.metrics
                .increment("messaging_send_429_received_total", self.channel, None)
                .await;

            if retries >= self.max_retries {
                self.metrics
                    .increment(
                        "messaging_send_retries_total",
                        self.channel,
                        Some("exhausted"),
                    )
                    .await;
                return Err(MoaError::RateLimited {
                    retries,
                    message: format!(
                        "{} send remained rate limited after {retries} retries: {}",
                        self.channel, response.body
                    ),
                });
            }

            retries += 1;
            let retry_after = response.retry_after();
            tracing::warn!(
                messaging.channel = %self.channel,
                http.status_code = response.status,
                retry_after_ms = retry_after.as_millis() as u64,
                attempt = retries,
                max_retries = self.max_retries,
                "messaging channel send was rate limited; retrying"
            );
            sleep(retry_after).await;
        }
    }

    /// Waits until this channel key has an available provider pacing slot.
    pub async fn wait_for_channel_slot(&self, channel_key: &str) -> Result<()> {
        if self.per_channel_interval.is_zero() {
            return Ok(());
        }

        if let Some(runtime_cache) = &self.runtime_cache {
            return self
                .wait_for_shared_channel_slot(runtime_cache.as_ref(), channel_key)
                .await;
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
        Ok(())
    }

    async fn wait_for_shared_channel_slot(
        &self,
        runtime_cache: &dyn RuntimeCacheStore,
        channel_key: &str,
    ) -> Result<()> {
        let cache_key = self.cache_key(channel_key);
        let interval_ms = duration_millis(self.per_channel_interval)?;

        loop {
            let now_ms = unix_millis_now()?;
            let current = runtime_cache.get(&cache_key).await?;
            let current_slot_ms = current
                .as_deref()
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.parse::<u64>().ok());
            let wait_until_ms = current_slot_ms
                .filter(|slot_ms| *slot_ms > now_ms)
                .unwrap_or_else(|| {
                    if current.is_none() && self.delay_first_send {
                        now_ms.saturating_add(interval_ms)
                    } else {
                        now_ms
                    }
                });
            let next_slot_ms = wait_until_ms.saturating_add(interval_ms);
            let next_value = next_slot_ms.to_string().into_bytes();

            if runtime_cache
                .compare_and_set(
                    &cache_key,
                    current.as_deref(),
                    next_value,
                    RATE_LIMIT_CACHE_TTL,
                )
                .await?
            {
                let now_after_claim = unix_millis_now()?;
                if wait_until_ms > now_after_claim {
                    sleep(Duration::from_millis(wait_until_ms - now_after_claim)).await;
                }
                return Ok(());
            }

            tokio::task::yield_now().await;
        }
    }

    fn cache_key(&self, channel_key: &str) -> String {
        format!("moa:messaging:rate_limit:{}:{channel_key}", self.channel)
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

fn unix_millis_now() -> Result<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    duration_millis(duration)
}

fn duration_millis(duration: Duration) -> Result<u64> {
    duration
        .as_millis()
        .try_into()
        .map_err(|_| MoaError::ValidationError("duration is too large".to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use moa_core::Channel;
    use moa_core::traits::RuntimeCacheStore;
    use moa_runtime_store::MemoryRuntimeCacheStore;

    use super::MessagingRateLimiter;

    #[tokio::test]
    async fn shared_runtime_cache_coordinates_channel_pacing_across_limiter_instances() {
        // Pins: two runtime instances share one Slack channel pacing slot through RuntimeCacheStore.
        let cache = Arc::new(MemoryRuntimeCacheStore::new());
        let first = MessagingRateLimiter::for_channel(Channel::Slack)
            .with_per_channel_interval(Duration::from_millis(40))
            .with_delay_first_send(false)
            .with_runtime_cache(cache.clone());
        let second = MessagingRateLimiter::for_channel(Channel::Slack)
            .with_per_channel_interval(Duration::from_millis(40))
            .with_delay_first_send(false)
            .with_runtime_cache(cache);

        first
            .wait_for_channel_slot("C123")
            .await
            .expect("first limiter should reserve a slot");
        let started = Instant::now();
        second
            .wait_for_channel_slot("C123")
            .await
            .expect("second limiter should observe the shared slot");

        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "second limiter did not wait on the shared Slack channel slot"
        );
    }

    #[tokio::test]
    async fn shared_runtime_cache_channel_pacing_records_wall_clock_slot() {
        // Pins: Redis-selected pacing stores Unix wall-clock slots through shared RuntimeCacheStore CAS.
        let cache = Arc::new(MemoryRuntimeCacheStore::new());
        let limiter = MessagingRateLimiter::for_channel(Channel::Slack)
            .with_per_channel_interval(Duration::from_millis(50))
            .with_delay_first_send(false)
            .with_runtime_cache(cache.clone());
        let before_ms = super::unix_millis_now().expect("wall clock should produce millis");

        limiter
            .wait_for_channel_slot("Cwall")
            .await
            .expect("limiter should reserve one shared wall-clock slot");

        let value = cache
            .get(&limiter.cache_key("Cwall"))
            .await
            .expect("shared runtime cache read should succeed")
            .expect("shared runtime cache should contain the reserved slot");
        let slot_ms = std::str::from_utf8(&value)
            .expect("slot should be stored as UTF-8 millis")
            .parse::<u64>()
            .expect("slot should parse as millis");
        let after_ms = super::unix_millis_now().expect("wall clock should produce millis");

        assert!(
            slot_ms >= before_ms + 50,
            "reserved slot {slot_ms} should be at least one interval after {before_ms}"
        );
        assert!(
            slot_ms <= after_ms + 50,
            "reserved slot {slot_ms} should be based on wall clock, not process Instant {after_ms}"
        );
    }
}
