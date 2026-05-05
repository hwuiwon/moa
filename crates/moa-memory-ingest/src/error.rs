//! Error types shared by graph-memory ingestion helpers.

use std::time::Duration;

/// Result type returned by ingestion helper functions.
pub type Result<T> = std::result::Result<T, IngestError>;

/// Errors returned by ingestion helper functions.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// The turn transcript is empty.
    #[error("turn transcript is empty")]
    EmptyTranscript,
    /// A requested chunk size was invalid.
    #[error("chunk token target must be greater than zero")]
    InvalidChunkTarget,
    /// The process-local ingestion runtime was not installed.
    #[error("ingestion runtime has not been installed")]
    RuntimeNotInstalled,
    /// A scoped Postgres helper failed.
    #[error("scope transaction: {0}")]
    Scope(#[from] moa_core::MoaError),
    /// A Postgres query failed.
    #[error("postgres: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Vector retrieval failed.
    #[error("vector: {0}")]
    Vector(#[from] moa_memory_vector::Error),
    /// Reranking failed.
    #[error("rerank: {0}")]
    Rerank(String),
    /// Judge execution failed.
    #[error("judge: {0}")]
    Judge(String),
    /// Contradiction detection failed.
    #[error("contradiction: {0}")]
    Contradiction(String),
    /// JSON serialization or parsing failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A contradiction detector budget expired.
    #[error("contradiction detector timed out after {0:?}")]
    Timeout(Duration),
}
