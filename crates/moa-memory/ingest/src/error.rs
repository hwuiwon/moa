//! Error types shared by graph-memory ingestion helpers.

use std::time::Duration;

/// Result type returned by ingestion helper functions.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by ingestion helper functions.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The turn transcript is empty.
    #[error("turn transcript is empty")]
    EmptyTranscript,
    /// A requested chunk size was invalid.
    #[error("chunk token target must be greater than zero")]
    InvalidChunkTarget,
    /// A chunk exceeded the deterministic extractor's configured size budget.
    #[error("chunk {index} has {actual_chars} chars, exceeding max {max_chars}")]
    ChunkTooLarge {
        /// Source chunk index.
        index: usize,
        /// Observed chunk length in Unicode scalar values.
        actual_chars: usize,
        /// Maximum allowed chunk length in Unicode scalar values.
        max_chars: usize,
    },
    /// Fact extraction failed.
    #[error("fact extraction: {0}")]
    Extraction(String),
    /// An embedding provider returned a different number of vectors than inputs,
    /// violating the provider cardinality contract. The whole batch is rejected
    /// rather than silently zipped (which would drop or misalign facts).
    #[error("embedding provider returned {actual} vectors for {expected} inputs")]
    EmbeddingCardinalityMismatch {
        /// Number of inputs sent to the embedding provider.
        expected: usize,
        /// Number of vectors the provider returned.
        actual: usize,
    },
    /// Model-backed memory inference failed.
    #[error("model inference: {0}")]
    ModelInference(String),
    /// PII classification abstained, so ingestion cannot safely persist the extracted text.
    #[error("pii classification unavailable from model version `{model_version}`")]
    PiiClassificationUnavailable {
        /// Model or serving version that produced the abstaining result.
        model_version: String,
    },
    /// The process-local ingestion runtime was not installed.
    #[error("ingestion runtime has not been installed")]
    RuntimeNotInstalled,
    /// A scoped Postgres helper failed.
    #[error("scope transaction: {0}")]
    Scope(#[from] moa_core::error::MoaError),
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
    /// Entity resolution failed.
    #[error("entity resolution: {0}")]
    EntityResolution(String),
    /// Graph storage failed.
    #[error("graph: {0}")]
    Graph(#[from] moa_memory_graph::Error),
    /// JSON serialization or parsing failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A contradiction detector budget expired.
    #[error("contradiction detector timed out after {0:?}")]
    Timeout(Duration),
}
