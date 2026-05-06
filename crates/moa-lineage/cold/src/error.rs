//! Error types for cold-tier lineage export.

/// Result type returned by cold-tier helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by cold-tier lineage export.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The exporter configuration is invalid.
    #[error("invalid cold-tier configuration: {0}")]
    Config(String),
    /// A Postgres operation failed.
    #[error("cold-tier postgres operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// An object-store operation failed.
    #[error("cold-tier object-store operation failed: {0}")]
    ObjectStore(#[from] object_store::Error),
    /// Arrow conversion failed.
    #[error("cold-tier arrow conversion failed: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
    /// Parquet writing failed.
    #[error("cold-tier parquet write failed: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
}
