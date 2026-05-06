//! Error type for lineage compliance audit helpers.

/// Result type used by `moa-lineage-audit`.
pub type Result<T> = std::result::Result<T, AuditError>;

/// Errors returned by audit-chain, signing, vault, export, and verification helpers.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// JSON canonicalization or serialization failed.
    #[error("audit json: {0}")]
    Json(#[from] serde_json::Error),
    /// UTF-8 conversion failed.
    #[error("audit utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    /// I/O failed.
    #[error("audit io: {0}")]
    Io(#[from] std::io::Error),
    /// Database query failed.
    #[error("audit postgres: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Object store operation failed.
    #[error("audit object store: {0}")]
    ObjectStore(#[from] object_store::Error),
    /// Zip bundle write failed.
    #[error("audit zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// Signature verification failed.
    #[error("audit signature verification failed")]
    Signature,
    /// Chain verification failed.
    #[error("audit chain mismatch at index {index}: {message}")]
    ChainMismatch {
        /// Failing record index.
        index: usize,
        /// Human-readable mismatch details.
        message: String,
    },
    /// The requested PII subject has been erased.
    #[error("pii subject has been crypto-shredded")]
    Erased,
    /// Invalid input.
    #[error("audit invalid input: {0}")]
    Invalid(String),
}
