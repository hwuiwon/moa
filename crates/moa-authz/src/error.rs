//! Error types for authorization client and outbox operations.

use thiserror::Error;

/// Errors returned by the OpenFGA client and transactional outbox.
#[derive(Debug, Error)]
pub enum AuthzError {
    /// OpenFGA returned a non-success HTTP status.
    #[error("FGA HTTP error: {status}: {body}")]
    HttpError {
        /// HTTP status code.
        status: u16,
        /// Response body returned by OpenFGA.
        body: String,
    },

    /// The HTTP client failed before receiving a valid OpenFGA response.
    #[error("FGA transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Postgres returned an error while reading or updating the outbox.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// SQLx migration metadata or execution failed.
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// JSON serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Required runtime configuration was missing or invalid.
    #[error("config error: {0}")]
    Config(String),

    /// OpenFGA returned a response that could not be interpreted safely.
    #[error("FGA returned ambiguous response: {0}")]
    Ambiguous(String),
}
