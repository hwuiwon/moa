//! Shared HTTP client helpers for provider implementations.

use std::time::Duration;

use moa_core::{error::MoaError, error::Result};
use reqwest::{Client, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Connection-establishment deadline shared by every provider HTTP client.
///
/// A bare `reqwest::Client` has no connect timeout, so a black-holed TCP handshake
/// can hang a provider call indefinitely; this bounds that phase for both the
/// streaming and non-streaming clients.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Whole-request deadline for non-streaming provider calls.
///
/// Only applied to request/response calls (embeddings, rerank) whose body is read
/// eagerly. Streaming LLM adapters must not use this because a legitimate long
/// generation stream would trip a whole-request timeout mid-response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

// The per-provider in-flight concurrency ceiling lives on each provider instance
// (see [`crate::core::concurrency::ConcurrencyLimiter`]), not on these shared
// client builders: the limit is per provider/tier and is acquired around the
// outbound call so it composes with each provider's rate pacer.

/// Builds the streaming HTTP client used by LLM adapters.
///
/// This client carries only a [`CONNECT_TIMEOUT`]; it deliberately has no
/// whole-request timeout so long server-sent-event generations are not aborted
/// mid-stream.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}

/// Builds the HTTP client used by non-streaming provider calls.
///
/// Embedding and rerank calls read their entire response body eagerly, so this
/// client adds a whole-request [`REQUEST_TIMEOUT`] on top of the shared
/// [`CONNECT_TIMEOUT`] to bound a stalled provider round trip.
pub(crate) fn build_json_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}

/// Sends a bearer-authenticated JSON POST and decodes the JSON response.
///
/// Transport failures and JSON decode failures map to
/// [`MoaError::ProviderError`]; non-success HTTP statuses map to
/// [`MoaError::HttpStatus`] carrying the response body.
pub(crate) async fn post_json<Req, Resp>(
    client: &Client,
    url: &str,
    bearer: &str,
    body: &Req,
) -> Result<Resp>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    let response = client
        .post(url)
        .bearer_auth(bearer)
        .json(body)
        .send()
        .await
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    decode_json_response(response).await
}

/// Decodes a JSON provider response, mapping non-success statuses to
/// [`MoaError::HttpStatus`] and decode failures to [`MoaError::ProviderError`].
pub(crate) async fn decode_json_response<Resp>(response: Response) -> Result<Resp>
where
    Resp: DeserializeOwned,
{
    let status = response.status();
    if !status.is_success() {
        let message = response
            .text()
            .await
            .unwrap_or_else(|error| format!("failed to read error body: {error}"));
        return Err(MoaError::HttpStatus {
            status: status.as_u16(),
            retry_after: None,
            message,
        });
    }

    response
        .json::<Resp>()
        .await
        .map_err(|error| MoaError::ProviderError(error.to_string()))
}

/// Rejects an embedding response whose row count differs from the request size.
pub(crate) fn validate_embedding_count(expected: usize, got: usize) -> Result<()> {
    if got != expected {
        return Err(MoaError::ProviderError(format!(
            "embedding response length mismatch: expected {expected}, got {got}"
        )));
    }
    Ok(())
}

/// Rejects an embedding whose width does not match the model's fixed dimension.
pub(crate) fn validate_embedding_dimension(expected: usize, got: &[f32]) -> Result<()> {
    if got.len() != expected {
        return Err(MoaError::ProviderError(format!(
            "embedding dimension mismatch: expected {expected}, got {}",
            got.len()
        )));
    }
    Ok(())
}
