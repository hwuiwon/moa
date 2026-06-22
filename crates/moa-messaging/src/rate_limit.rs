//! Rate-limit retry and per-channel send pacing for messaging adapters.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{Channel, MoaError, Result};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};

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

        let retry_after = self.retry_after_for_channel_opt(channel);
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

    fn retry_after_for_channel(&self, channel: Channel) -> Duration {
        self.retry_after_for_channel_opt(channel)
            .unwrap_or_else(|| Duration::from_secs(1))
    }

    fn retry_after_for_channel_opt(&self, _channel: Channel) -> Option<Duration> {
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
#[derive(Debug, Clone)]
pub struct MessagingRateLimiter {
    channel: Channel,
    max_retries: usize,
    per_channel_interval: Duration,
    delay_first_send: bool,
    next_send_at: Arc<Mutex<HashMap<String, Instant>>>,
    metrics: Arc<MessagingRateLimitMetrics>,
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
            self.wait_for_channel_slot(channel_key).await;
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
            let retry_after = response.retry_after_for_channel(self.channel);
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

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}
