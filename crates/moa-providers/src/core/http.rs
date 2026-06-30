//! Shared HTTP client helpers for provider implementations.

use moa_core::{MoaError, Result};
use reqwest::{Client, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Builds the shared HTTP client used by provider implementations.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
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
