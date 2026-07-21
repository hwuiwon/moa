//! Error type for graph-memory operations.

/// Error returned by the graph-memory crate.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// A relational graph query failed or returned an unexpected shape.
    #[error("graph query: {0}")]
    GraphQuery(String),
    /// A SQL sidecar query failed.
    #[error("sidecar: {0}")]
    Sidecar(#[from] sqlx::Error),
    /// A target node or edge was not found.
    #[error("not found: {0}")]
    NotFound(uuid::Uuid),
    /// Row-level security denied the operation.
    #[error("rls denied")]
    RlsDenied,
    /// A graph write was attempted without a request scope.
    #[error("graph writes require a scoped connection")]
    MissingScope,
    /// A node intent supplied an embedding without complete embedding metadata.
    #[error("embedding requires model name and model version")]
    MissingEmbeddingMetadata,
    /// Restricted or PHI content was supplied with an embedding.
    #[error("restricted/PHI graph nodes cannot have embeddings")]
    SealedEmbedding,
    /// A node's explicit data subject does not match its typed tenant/contact scope.
    #[error("data subject `{actual}` does not match scope subject `{expected}`")]
    DataSubjectMismatch {
        /// Explicit subject supplied by the caller.
        actual: uuid::Uuid,
        /// Subject required by the row's tenant/contact scope.
        expected: uuid::Uuid,
    },
    /// The requested mutation conflicts with current graph state.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The requested mutation violates bitemporal validity rules.
    #[error("bi-temporal violation: {0}")]
    BiTemporal(String),
    /// A node label stored in Postgres is not part of the supported label set.
    #[error("unknown node label `{0}`")]
    UnknownNodeLabel(String),
    /// An edge label stored in Postgres is not part of the supported label set.
    #[error("unknown edge label `{0}`")]
    UnknownEdgeLabel(String),
    /// A changelog record's explicit scope does not match its storage-partition/user shape.
    #[error("changelog scope `{actual}` does not match computed scope `{expected}`")]
    ChangelogScopeMismatch {
        /// Caller-provided scope string.
        actual: String,
        /// Scope computed from `storage_partition_id` and `user_id`.
        expected: &'static str,
    },
    /// A changelog record used an unsupported storage-partition/user shape.
    #[error("changelog user scope requires a storage_partition_id")]
    InvalidChangelogScope,
    /// A scoped Postgres transaction could not be started or committed.
    #[error("scope transaction: {0}")]
    Scope(#[from] moa_core::error::MoaError),
    /// A vector store operation failed.
    #[error("vector store: {0}")]
    Vector(#[from] moa_memory_vector::Error),
    /// JSON serialization for audit payload hashing failed.
    #[error("json serialization: {0}")]
    Json(#[from] serde_json::Error),
    /// Envelope encryption or decryption of restricted node content failed.
    ///
    /// Sealing (write path) and opening (read path) restricted/PHI `name` and
    /// `properties_summary` are wrapped here. A crypto-shredded subject is not an
    /// error: the read path maps it to the redaction placeholder instead.
    #[error("crypto: {0}")]
    Crypto(#[from] moa_crypto::Error),
    /// A resumable sealed-content backfill found an invalid historical row.
    #[error("sealed-content backfill: {0}")]
    Backfill(String),
}
