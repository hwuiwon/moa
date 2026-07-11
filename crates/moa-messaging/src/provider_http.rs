//! Shared HTTP send/retry/error helpers for the Postmark and Twilio connectors.
//!
//! Both REST connectors share the same `Retry-After` parsing, retryable-status
//! classification, response-body reading, field-validation, and span-recording
//! logic. This crate-private module holds the single implementation so each
//! connector keeps only its provider-specific request shaping and decoding.

use std::time::Duration;

use moa_core::{error::MoaError, error::Result};
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};

use crate::rate_limit::parse_retry_after;

/// Connection-establishment deadline for the messaging REST connectors.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request deadline for the messaging REST connectors.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Builds the default REST connector HTTP client with connect and request timeouts.
///
/// A bare `reqwest::Client::new()` has no timeouts, so a stalled Postmark or
/// Twilio round trip could hang a notification send indefinitely. This bounds
/// both the connect and whole-request phases, falling back to the untimed client
/// only if the builder itself fails (which it does not for a static config).
pub(crate) fn default_http_client() -> Client {
    Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// Reads a response body, substituting a diagnostic string when the body cannot be read.
pub(crate) async fn response_text(response: reqwest::Response) -> String {
    response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read response body: {error}"))
}

/// Returns true when the HTTP status is one a durable caller may safely retry.
pub(crate) fn is_retryable_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Extracts a `Retry-After` delay from response headers when present and parseable.
pub(crate) fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value)
}

/// Returns the trimmed value when non-empty, otherwise `None`.
pub(crate) fn optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Returns the trimmed value or a validation error reading `"{context} {name} is required"`.
pub(crate) fn required_field(context: &str, name: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(MoaError::ValidationError(format!(
            "{context} {name} is required"
        )));
    }
    Ok(trimmed.to_string())
}

/// Records connector API-error fields on the current span using the provider's field prefix.
///
/// `prefix` selects the per-provider field namespace (for example `"postmark"`
/// or `"twilio"`) so each connector still emits its exact span fields such as
/// `<prefix>.error_code`, `<prefix>.retry_after_ms`, `<prefix>.failure_class`,
/// and `<prefix>.retryable`. The shared `http.status_code` and `error` fields
/// are recorded verbatim.
pub(crate) fn record_api_error(
    prefix: &str,
    status: Option<StatusCode>,
    error_code: Option<i64>,
    retry_after: Option<Duration>,
    retryable: bool,
    message: &str,
) {
    let span = tracing::Span::current();
    if let Some(status) = status {
        span.record("http.status_code", status.as_u16());
    }
    if let Some(error_code) = error_code {
        span.record(format!("{prefix}.error_code").as_str(), error_code);
    }
    if let Some(retry_after) = retry_after {
        span.record(
            format!("{prefix}.retry_after_ms").as_str(),
            retry_after.as_millis() as u64,
        );
    }
    let failure_class = if retryable { "retryable" } else { "permanent" };
    span.record(format!("{prefix}.failure_class").as_str(), failure_class);
    span.record(format!("{prefix}.retryable").as_str(), retryable);
    span.record("error", message);
}
