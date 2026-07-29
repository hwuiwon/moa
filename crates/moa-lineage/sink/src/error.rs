//! Error type for the lineage sink.

/// Result type used by `moa-lineage-sink`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by lineage sink setup, journaling, and writes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQL write failed.
    #[error("lineage sql: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// ClickHouse operation failed.
    #[error("lineage clickhouse: {0}")]
    ClickHouse(#[from] clickhouse::error::Error),
    /// JSON serialization failed.
    #[error("lineage json: {0}")]
    Json(#[from] serde_json::Error),
    /// Lineage hash-chain canonicalization or verification failed.
    #[error("lineage hash chain: {0}")]
    Chain(#[from] moa_lineage_core::chain::LineageChainError),
    /// Writer task join failed.
    #[error("lineage writer join: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Invalid lineage payload or audit state.
    #[error("invalid lineage sink input: {0}")]
    Invalid(String),
    /// A queue lease expired and another writer reclaimed part or all of the batch.
    #[error(
        "lineage queue lease lost before terminal update: expected {expected} rows, owned {owned}"
    )]
    LeaseLost {
        /// Number of rows in the original claim.
        expected: u64,
        /// Number still owned by this writer.
        owned: u64,
    },
    /// Graceful shutdown exhausted its configured drain budget.
    #[error("lineage writer drain exceeded its {timeout_ms}ms shutdown budget")]
    DrainTimeout {
        /// Configured shutdown budget in milliseconds.
        timeout_ms: u64,
    },
    /// A replayed experiment score carried different provenance than the stored row.
    ///
    /// Replay of an identical score is accepted as a no-op. A score that keeps its
    /// identity while changing what it claims to have observed is a different
    /// score wearing the same name, and it is refused rather than absorbed.
    #[error(
        "{count} experiment score(s) replayed with provenance that differs from the stored row"
    )]
    ExperimentScoreProvenanceConflict {
        /// Number of colliding score rows in the batch.
        count: i64,
    },
}
