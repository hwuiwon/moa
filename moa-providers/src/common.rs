//! Shared HTTP and SSE utilities for provider implementations.

use std::time::Duration;

use eventsource_stream::Event as SseEvent;
use moa_core::{MoaError, Result};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;

const INITIAL_RETRY_DELAY_MS: u64 = 250;

/// Builds the shared HTTP client used by provider implementations.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}

/// Sends a request with retry handling for provider rate limits.
pub(crate) async fn send_with_retry<F>(build_request: F, max_retries: usize) -> Result<Response>
where
    F: Fn() -> RequestBuilder,
{
    let mut attempt = 0usize;

    loop {
        let response = build_request().send().await.map_err(|error| {
            MoaError::ProviderError(format!("provider request failed: {error}"))
        })?;

        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let message = response_text(response).await;
            if attempt >= max_retries {
                return Err(MoaError::RateLimited {
                    retries: max_retries,
                    message,
                });
            }

            let delay = retry_delay(attempt);
            tracing::warn!(
                attempt = attempt + 1,
                max_retries,
                delay_ms = delay.as_millis(),
                "provider request hit a rate limit; retrying"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
            continue;
        }

        if !status.is_success() {
            let message = response_text(response).await;
            return Err(MoaError::HttpStatus {
                status: status.as_u16(),
                message,
            });
        }

        return Ok(response);
    }
}

/// Parses a JSON SSE payload into a strongly typed Rust value.
pub(crate) fn parse_sse_json<T>(event: &SseEvent) -> Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(&event.data).map_err(|error| {
        MoaError::SerializationError(format!(
            "failed to parse SSE payload for event '{}': {error}",
            event.event
        ))
    })
}

/// Returns the exponential backoff delay for a retry attempt.
pub(crate) fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(INITIAL_RETRY_DELAY_MS.saturating_mul(1_u64 << attempt.min(8)))
}

async fn response_text(response: Response) -> String {
    match response.text().await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "request failed with an empty response body".to_string(),
        Err(error) => format!("request failed and the response body could not be read: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use reqwest::StatusCode;

    use super::{build_http_client, send_with_retry};

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

        let client = build_http_client().unwrap();
        let url = format!("http://{address}/retry");
        let response = send_with_retry(|| client.get(&url), 3).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);

        server.abort();
    }
}
