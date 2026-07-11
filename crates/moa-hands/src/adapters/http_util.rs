//! Shared HTTP request and error helpers for cloud sandbox adapters.

use std::time::Duration;

use moa_core::{error::MoaError, error::Result};
use reqwest::header::RETRY_AFTER;
use serde_json::Value;

/// Parses the JSON body of a successful response, mapping failures to errors.
///
/// `provider` is the human-readable backend name embedded in error messages.
pub(crate) async fn expect_success_json(
    response: reqwest::Response,
    provider: &str,
) -> Result<Value> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    response.json::<Value>().await.map_err(|error| {
        MoaError::ProviderError(format!("invalid {provider} JSON response: {error}"))
    })
}

/// Returns an error when the response is not successful, discarding the body otherwise.
pub(crate) async fn expect_success(response: reqwest::Response) -> Result<()> {
    if !response.status().is_success() {
        return Err(http_error(response).await);
    }
    Ok(())
}

/// Builds a structured [`MoaError::HttpStatus`] from a failed response.
pub(crate) async fn http_error(response: reqwest::Response) -> MoaError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after);
    let message = response
        .text()
        .await
        .unwrap_or_else(|_| "failed to read response body".to_string());
    MoaError::HttpStatus {
        status,
        retry_after,
        message,
    }
}

/// Parses a base URL and appends the provided query parameters.
///
/// `provider` is the human-readable backend name embedded in error messages.
pub(crate) fn build_url(
    base: &str,
    params: &[(&str, &str)],
    provider: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base).map_err(|error| {
        MoaError::ValidationError(format!("invalid {provider} URL {base}: {error}"))
    })?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Extracts a required string field from a JSON object.
pub(crate) fn required_string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| MoaError::ValidationError(format!("missing string field `{field}`")))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}
