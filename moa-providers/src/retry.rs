//! Shared retry and backoff policy for provider HTTP requests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use moa_core::{MoaError, Result};
use reqwest::{
    RequestBuilder, Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};

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

    /// Sends an HTTP request with exponential backoff and jitter on retryable failures.
    pub(crate) async fn send<F>(&self, build_request: F) -> Result<Response>
    where
        F: Fn() -> RequestBuilder,
    {
        let mut attempt = 0usize;

        loop {
            let response = match build_request().send().await {
                Ok(response) => response,
                Err(error) => {
                    if self.is_retryable_transport_error(&error) && attempt < self.max_retries {
                        let delay = self.delay_for_attempt(attempt);
                        tracing::warn!(
                            attempt = attempt + 1,
                            max_retries = self.max_retries,
                            delay_ms = delay.as_millis(),
                            error = %error,
                            "provider request failed with a retryable transport error; retrying"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
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
            if Self::is_retryable_status(status) && attempt < self.max_retries {
                let delay = retry_after_delay_from_message(&message)
                    .or_else(|| retry_after_delay(status, Some(&headers)))
                    .unwrap_or_else(|| self.delay_for_attempt(attempt));
                tracing::warn!(
                    attempt = attempt + 1,
                    max_retries = self.max_retries,
                    delay_ms = delay.as_millis(),
                    status = status.as_u16(),
                    message,
                    "provider request returned a retryable HTTP status; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
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
        error.is_timeout() || error.is_connect() || error.is_request()
    }

    fn jitter_seed(&self) -> f64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.subsec_nanos())
            .unwrap_or(500_000_000);
        nanos as f64 / 1_000_000_000.0
    }
}

fn retry_after_delay(status: StatusCode, headers: Option<&HeaderMap>) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }

    let value = headers?.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value)
}

fn retry_after_delay_from_message(_message: &str) -> Option<Duration> {
    None
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

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::RetryPolicy;

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
            .send(|| client.get(&url))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        server.abort();
    }
}
