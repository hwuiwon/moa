//! Shared retry and backoff policy for provider HTTP requests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use moa_core::{error::MoaError, error::Result};
use opentelemetry::KeyValue;
use reqwest::{
    RequestBuilder, Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::core::rate_guard::{RateGuard, RateLimitScope};

/// Records a bounded `provider_retry` span event on the ambient span (the
/// caller's `llm_completion`/embedding/rerank span, entered via
/// `tracing::Instrument`) so retry attempts are visible without re-deriving
/// them from log lines. `reason` must be a small, bounded label — never a raw
/// provider error message.
fn record_retry_attempt(attempt: usize, reason: &'static str) {
    tracing::Span::current().add_event(
        "provider_retry",
        vec![
            KeyValue::new("retry.attempt", (attempt + 1) as i64),
            KeyValue::new("error.type", reason),
        ],
    );
}

/// Maps a retryable HTTP status to a bounded `error.type` label for the retry
/// span event. Only [`RetryPolicy::is_retryable_status`] statuses reach this
/// function.
fn retry_status_class(status: StatusCode) -> &'static str {
    match status {
        StatusCode::TOO_MANY_REQUESTS => "http_429",
        StatusCode::INTERNAL_SERVER_ERROR => "http_500",
        StatusCode::BAD_GATEWAY => "http_502",
        StatusCode::SERVICE_UNAVAILABLE => "http_503",
        StatusCode::GATEWAY_TIMEOUT => "http_504",
        _ => "http_error",
    }
}

/// Shared retry policy for provider HTTP requests.
#[derive(Debug, Clone)]
pub(crate) struct RetryPolicy {
    /// Maximum number of retry attempts after the initial request.
    pub(crate) max_retries: usize,
    /// Base delay for exponential backoff.
    pub(crate) initial_delay: Duration,
    /// Upper bound for exponential backoff delay.
    pub(crate) max_delay: Duration,
    /// Exponential backoff multiplier.
    pub(crate) backoff_factor: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
        }
    }
}

impl RetryPolicy {
    /// Returns a copy of the policy with an overridden retry budget.
    pub(crate) fn with_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Sends an HTTP request with exponential backoff and jitter, consulting a
    /// [`RateGuard`]: in-call retries stop early once the retry budget is spent,
    /// and a terminal 429 records the provider's cooldown so later calls can
    /// short-circuit or fail over instead of hammering the endpoint.
    pub(crate) async fn send_gated<F>(
        &self,
        build_request: F,
        guard: &RateGuard,
    ) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        let mut attempt = 0usize;

        loop {
            let response = match build_request().send().await {
                Ok(response) => response,
                Err(error) => {
                    let retry_eligible =
                        self.is_retryable_transport_error(&error) && attempt < self.max_retries;
                    if retry_eligible && guard.allow_retry().await {
                        let delay = self.delay_for_attempt(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_retries = self.max_retries,
                            delay_ms = delay.as_millis(),
                            error = %error,
                            "provider request failed with a retryable transport error; retrying"
                        );
                        record_retry_attempt(attempt, "transport_error");
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    if retry_eligible {
                        record_retry_budget_exhausted("transport");
                    }

                    return Err(MoaError::ProviderError(format!(
                        "provider request failed: {error}"
                    )));
                }
            };

            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }

            let headers = response.headers().clone();
            let message = response_text(response).await;
            let rate_limit_delay = (status == StatusCode::TOO_MANY_REQUESTS)
                .then(|| {
                    retry_after_delay_from_message(&message)
                        .or_else(|| retry_after_delay(status, Some(&headers)))
                })
                .flatten();
            if status == StatusCode::TOO_MANY_REQUESTS {
                // Pause the shared provider guard immediately. This protects
                // concurrent calls even when this request spends its own retry
                // budget and later succeeds.
                // This path sees a status, headers, and an opaque body — not the
                // vendor taxonomy that would say whether the limit is the
                // model's or the whole account's. Record the broader scope.
                guard
                    .record_rate_limited(rate_limit_delay, RateLimitScope::unclassified())
                    .await;
            }
            let retry_eligible = Self::is_retryable_status(status) && attempt < self.max_retries;
            if retry_eligible && guard.allow_retry().await {
                let delay = self.retry_delay(rate_limit_delay, attempt);
                tracing::warn!(
                    attempt = attempt + 1,
                    max_retries = self.max_retries,
                    delay_ms = delay.as_millis(),
                    status = status.as_u16(),
                    message,
                    "provider request returned a retryable HTTP status; retrying"
                );
                record_retry_attempt(attempt, retry_status_class(status));
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }
            if retry_eligible {
                record_retry_budget_exhausted(status.as_str());
            }

            if status == StatusCode::TOO_MANY_REQUESTS {
                return Err(MoaError::RateLimited {
                    retries: self.max_retries,
                    message,
                });
            }

            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                retry_after: retry_after_delay(status, Some(&headers)),
                message,
            });
        }
    }

    /// Returns the delay before the next in-call retry.
    ///
    /// A server-supplied `Retry-After` (`rate_limit_delay`) is honored but capped
    /// at [`max_delay`](Self::max_delay), so a hostile or misconfigured header
    /// cannot pin a retry far beyond the exponential-backoff ceiling; when no
    /// header is present, exponential backoff for `attempt` is used.
    pub(crate) fn retry_delay(
        &self,
        rate_limit_delay: Option<Duration>,
        attempt: usize,
    ) -> Duration {
        match rate_limit_delay {
            Some(delay) => delay.min(self.max_delay),
            None => self.delay_for_attempt(attempt),
        }
    }

    /// Returns the exponential backoff delay for one retry attempt.
    pub(crate) fn delay_for_attempt(&self, attempt: usize) -> Duration {
        let base = self.initial_delay.as_secs_f64() * self.backoff_factor.powi(attempt as i32);
        let capped = base.min(self.max_delay.as_secs_f64());
        let jitter = capped * (0.5 + self.jitter_seed() * 0.5);
        Duration::from_secs_f64(jitter)
    }

    /// Returns whether the provided HTTP status should be retried.
    pub(crate) fn is_retryable_status(status: StatusCode) -> bool {
        matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        )
    }

    fn is_retryable_transport_error(&self, error: &reqwest::Error) -> bool {
        error.is_connect()
    }

    fn jitter_seed(&self) -> f64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(500_000_000);
        nanos as f64 / 1_000_000_000.0
    }
}

/// Records that an otherwise-eligible retry was denied by the provider retry
/// budget, tagged with the triggering `kind` (an HTTP status code or
/// `"transport"`), so sustained budget exhaustion is observable in metrics.
fn record_retry_budget_exhausted(kind: &str) {
    metrics::counter!("moa_llm_retry_budget_exhausted_total", "kind" => kind.to_string())
        .increment(1);
}

fn retry_after_delay(status: StatusCode, headers: Option<&HeaderMap>) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }

    let value = headers?.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value)
}

fn retry_after_delay_from_message(message: &str) -> Option<Duration> {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|value| retry_after_delay_from_json(&value))
        .or_else(|| retry_after_delay_from_text(message))
}

fn retry_after_delay_from_json(value: &Value) -> Option<Duration> {
    match value {
        Value::Object(map) => {
            for key in [
                "retry_after",
                "retry_after_seconds",
                "retryAfter",
                "retry_after_ms",
                "retryAfterMs",
            ] {
                let parser = if key.ends_with("_ms") || key.ends_with("Ms") {
                    retry_after_millis_from_json_scalar
                } else {
                    retry_after_delay_from_json_scalar
                };
                if let Some(delay) = map.get(key).and_then(parser) {
                    return Some(delay);
                }
            }
            map.values().find_map(retry_after_delay_from_json)
        }
        Value::Array(values) => values.iter().find_map(retry_after_delay_from_json),
        _ => None,
    }
}

fn retry_after_delay_from_json_scalar(value: &Value) -> Option<Duration> {
    match value {
        Value::Number(number) => number.as_u64().map(Duration::from_secs),
        Value::String(value) => parse_retry_after(value),
        _ => None,
    }
}

fn retry_after_millis_from_json_scalar(value: &Value) -> Option<Duration> {
    match value {
        Value::Number(number) => number.as_u64().map(Duration::from_millis),
        Value::String(value) => parse_retry_after_millis_or_seconds(value),
        _ => None,
    }
}

fn retry_after_delay_from_text(message: &str) -> Option<Duration> {
    for marker in ["retry-after=", "retry_after=", "retryAfter="] {
        if let Some((_, rest)) = message.split_once(marker) {
            let value = rest
                .split(|ch: char| ch.is_whitespace() || ch == ')' || ch == ',' || ch == ';')
                .next()
                .unwrap_or_default()
                .trim();
            if let Some(delay) = parse_retry_after_millis_or_seconds(value) {
                return Some(delay);
            }
        }
    }
    None
}

fn parse_retry_after_millis_or_seconds(value: &str) -> Option<Duration> {
    if let Some(ms) = value.strip_suffix("ms") {
        return ms.trim().parse::<u64>().ok().map(Duration::from_millis);
    }
    parse_retry_after(value)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let reset_at = DateTime::parse_from_rfc2822(value).ok()?;
    let reset_at = reset_at.with_timezone(&Utc);
    let remaining = reset_at.signed_duration_since(Utc::now());
    let millis = remaining.num_milliseconds().max(0) as u64;
    Some(Duration::from_millis(millis))
}

async fn response_text(response: Response) -> String {
    let headers = response.headers().clone();
    let status = response.status();
    match response.text().await {
        Ok(text) if !text.trim().is_empty() => {
            if let Some(delay) = retry_after_delay(status, Some(&headers)) {
                format!("{text} (retry-after={}ms)", delay.as_millis())
            } else {
                text
            }
        }
        Ok(_) => "request failed with an empty response body".to_string(),
        Err(error) => format!("request failed and the response body could not be read: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use moa_core::error::MoaError;

    use moa_config::ProviderPacingConfig;

    use super::{RateGuard, RetryPolicy, retry_after_delay_from_message};

    #[tokio::test]
    async fn retries_on_rate_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let current = request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;

                let response = if current == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 11\r\n\r\nrate limit"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                };

                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{address}/retry");
        let response = RetryPolicy::default()
            .with_max_retries(3)
            .send_gated(
                || client.get(&url),
                &RateGuard::new(ProviderPacingConfig::default()),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn retry_emits_bounded_span_event_on_rate_limit() {
        // Pins: each in-call retry records a bounded `provider_retry` span
        // event (retry.attempt + a bounded error.type) on the ambient span,
        // not just a log line, so retries are visible in traces without
        // re-deriving them from logs.
        use tracing::Instrument;

        use crate::core::span_capture_test_support::{capture_spans_async, find_span};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let current = request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;

                let response = if current == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 11\r\n\r\nrate limit"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                };

                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{address}/retry");

        let spans = capture_spans_async(async {
            let span = tracing::info_span!("test_retry_span");
            RetryPolicy::default()
                .with_max_retries(3)
                .send_gated(
                    || client.get(&url),
                    &RateGuard::new(ProviderPacingConfig::default()),
                )
                .instrument(span)
                .await
                .unwrap();
        })
        .await;

        // The ambient span also picks up unrelated TRACE-level events emitted
        // by hyper/tokio internals during the real HTTP round trip, so filter
        // to the `provider_retry` events this change actually adds.
        let span = find_span(&spans, "test_retry_span");
        let retry_events = span
            .events
            .iter()
            .filter(|event| event.name == "provider_retry")
            .collect::<Vec<_>>();
        assert_eq!(
            retry_events.len(),
            1,
            "expected exactly one provider_retry event, got {:?}",
            retry_events
        );
        let event = retry_events[0];
        let attempt = event
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "retry.attempt")
            .expect("retry.attempt attribute present");
        assert_eq!(attempt.value, opentelemetry::Value::I64(1));
        let error_type = event
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "error.type")
            .expect("error.type attribute present");
        assert_eq!(error_type.value.as_str(), "http_429");

        server.abort();
    }

    #[tokio::test]
    async fn retried_rate_limit_records_cooldown_immediately() {
        // Pins: even when the in-call retry succeeds, the first 429 pauses the
        // shared provider guard so concurrent calls can fail over instead of
        // continuing into a provider that just advertised a cooldown.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let current = request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;

                let response = if current == 0 {
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 11\r\n\r\nrate limit"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"
                };

                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let guard = RateGuard::new(ProviderPacingConfig::default());
        let client = reqwest::Client::new();
        let url = format!("http://{address}/retry");
        let response = RetryPolicy {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            backoff_factor: 1.0,
        }
        .send_gated(|| client.get(&url), &guard)
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            guard.pause_remaining().await.expect("read pause").is_some(),
            "the guard must be paused by the first 429 even when a retry later succeeds"
        );

        server.abort();
    }

    #[tokio::test]
    async fn does_not_retry_after_ambiguous_post_transport_failure() {
        // Pins: POST failures after the request reaches the server are not replayed blindly.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{address}/ambiguous-post");
        let policy = RetryPolicy {
            max_retries: 3,
            initial_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
            backoff_factor: 1.0,
        };

        let result = policy
            .send_gated(
                || client.post(&url).body("payload"),
                &RateGuard::new(ProviderPacingConfig::default()),
            )
            .await;

        assert!(result.is_err());
        assert_eq!(request_count.load(Ordering::SeqCst), 1);

        server.abort();
    }

    #[tokio::test]
    async fn exhausted_retry_budget_fails_fast_without_retrying() {
        // Pins: once the retry budget is spent, send_gated does not retry a
        // retryable status; it fails fast (recording the budget-exhausted counter)
        // so callers can fail over instead of hammering the endpoint.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_task = Arc::clone(&request_count);

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                request_count_task.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response =
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 10\r\n\r\nrate limit";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let guard = RateGuard::new(ProviderPacingConfig::default());
        // Drain the retry budget so the next eligible retry is denied.
        while guard.allow_retry().await {}

        let client = reqwest::Client::new();
        let url = format!("http://{address}/exhausted");
        let result = RetryPolicy::default()
            .with_max_retries(3)
            .send_gated(|| client.get(&url), &guard)
            .await;

        assert!(matches!(result, Err(MoaError::RateLimited { .. })));
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            1,
            "an exhausted retry budget must not drive additional requests"
        );

        server.abort();
    }

    #[tokio::test]
    async fn a_429_from_the_real_http_path_pauses_the_whole_credential() {
        // Pins the conservative cooldown scope on the path that actually records
        // it. The guard-level tests prove credential scope pauses every model;
        // this proves the shared HTTP path CHOOSES credential scope, because
        // nothing here can tell a model-scoped rate limit from account-level
        // quota exhaustion. Without this, narrowing the recording call to the
        // called model would let the credential's other models keep hammering an
        // exhausted key with no test failing.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0_u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response =
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 10\r\n\r\nrate limit";
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let guard = RateGuard::new(ProviderPacingConfig::default());
        let called = guard.for_model("claude-opus-4-6");
        let untouched = guard.for_model("claude-haiku-4-5");
        assert!(untouched.pause_remaining().await.expect("read").is_none());

        let client = reqwest::Client::new();
        let url = format!("http://{address}/rate-limited");
        let result = RetryPolicy::default()
            .with_max_retries(0)
            .send_gated(|| client.get(&url), &called)
            .await;
        assert!(matches!(result, Err(MoaError::RateLimited { .. })));

        assert!(
            untouched.pause_remaining().await.expect("read").is_some(),
            "a 429 with no classifying evidence must pause every model on the credential"
        );

        server.abort();
    }

    #[test]
    fn retry_after_is_capped_at_max_delay() {
        // Pins: a server-supplied Retry-After above the backoff ceiling is capped
        // at max_delay, a smaller hint is honored as-is, and an absent hint falls
        // back to bounded exponential backoff.
        let policy = RetryPolicy {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
        };
        assert_eq!(
            policy.retry_delay(Some(Duration::from_secs(3600)), 0),
            Duration::from_secs(60),
            "a hostile Retry-After must be capped at max_delay"
        );
        assert_eq!(
            policy.retry_delay(Some(Duration::from_secs(5)), 0),
            Duration::from_secs(5),
            "a Retry-After within the ceiling is honored as-is"
        );
        assert!(
            policy.retry_delay(None, 5) <= Duration::from_secs(60),
            "the exponential fallback never exceeds max_delay"
        );
    }

    #[test]
    fn retry_after_delay_from_message_parses_structured_seconds() {
        // Pins: provider body hints can drive retry delay when Retry-After headers are absent.
        let delay = retry_after_delay_from_message(
            r#"{"error":{"message":"rate limited","retry_after":3}}"#,
        );

        assert_eq!(delay, Some(Duration::from_secs(3)));
    }

    #[test]
    fn retry_after_delay_from_message_parses_embedded_milliseconds() {
        // Pins: existing response-text annotations are parsed instead of ignored.
        let delay = retry_after_delay_from_message("rate limited (retry-after=250ms)");

        assert_eq!(delay, Some(Duration::from_millis(250)));
    }
}
