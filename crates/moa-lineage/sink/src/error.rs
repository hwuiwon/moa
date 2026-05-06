//! Error type for the lineage sink.

/// Result type used by `moa-lineage-sink`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by lineage sink setup, journaling, and writes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// fjall durable journal operation failed.
    #[error("fjall journal: {0}")]
    Journal(#[from] fjall::Error),
    /// SQL write failed.
    #[error("lineage sql: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// JSON serialization failed.
    #[error("lineage json: {0}")]
    Json(#[from] serde_json::Error),
    /// Writer task join failed.
    #[error("lineage writer join: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Journal key had an unexpected shape.
    #[error("invalid lineage journal key")]
    InvalidJournalKey,
    /// Invalid lineage payload or audit state.
    #[error("invalid lineage sink input: {0}")]
    Invalid(String),
}
