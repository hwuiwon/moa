//! Shared HTTP client helpers for provider implementations.

use moa_core::{MoaError, Result};
use reqwest::Client;

/// Builds the shared HTTP client used by provider implementations.
pub(crate) fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("moa/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| MoaError::ProviderError(format!("failed to build HTTP client: {error}")))
}
