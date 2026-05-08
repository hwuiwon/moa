//! Error types returned by the orchestrator thin client.

use thiserror::Error;

/// Result alias for orchestrator client operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Error returned by orchestrator client operations.
#[derive(Debug, Error)]
pub enum Error {
    /// No orchestrator endpoint was configured for a strict endpoint lookup.
    #[error("orchestrator endpoint not configured (set MOA__ORCHESTRATOR__ENDPOINT)")]
    EndpointNotConfigured,
    /// The configured endpoint was not a valid URL.
    #[error("orchestrator endpoint URL invalid: {0}")]
    InvalidEndpoint(String),
    /// The HTTP request failed before a successful response body was available.
    #[error("network error talking to orchestrator: {0}")]
    Network(#[from] reqwest::Error),
    /// Restate ingress returned a non-success status.
    #[error("orchestrator returned bad status {status}: {body}")]
    BadStatus {
        /// HTTP response status.
        status: reqwest::StatusCode,
        /// Response body returned by Restate ingress.
        body: String,
    },
    /// A successful response body did not match the expected wire type.
    #[error("failed to decode orchestrator response: {0}")]
    Decode(#[from] serde_json::Error),
    /// A polling operation exceeded its caller-provided deadline.
    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),
    /// The caller cancelled the client-side operation.
    #[error("operation cancelled by caller")]
    Cancelled,
}
