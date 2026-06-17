//! Error types for experiment domain and storage operations.

/// Experiment crate error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Postgres or SQLx operation failed.
    #[error("experiment storage operation failed")]
    Storage(#[from] sqlx::Error),
    /// JSON serialization or deserialization failed.
    #[error("experiment JSON serialization failed")]
    Json(#[from] serde_json::Error),
}

/// Result type returned by experiment crate operations.
pub type Result<T> = std::result::Result<T, Error>;
