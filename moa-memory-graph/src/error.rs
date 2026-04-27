//! Error type for graph-memory operations.

/// Error returned by the graph-memory crate.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// An Apache AGE Cypher query failed or returned an unexpected shape.
    #[error("cypher: {0}")]
    Cypher(String),
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
    /// A PII class stored in Postgres is not part of the supported class set.
    #[error("unknown PII class `{0}`")]
    UnknownPiiClass(String),
    /// A changelog record's explicit scope does not match its workspace/user shape.
    #[error("changelog scope `{actual}` does not match computed scope `{expected}`")]
    ChangelogScopeMismatch {
        /// Caller-provided scope string.
        actual: String,
        /// Scope computed from `workspace_id` and `user_id`.
        expected: &'static str,
    },
    /// A changelog record used an unsupported workspace/user shape.
    #[error("changelog user scope requires a workspace_id")]
    InvalidChangelogScope,
    /// A scoped Postgres transaction could not be started or committed.
    #[error("scope transaction: {0}")]
    Scope(#[from] moa_core::MoaError),
    /// A vector store operation failed.
    #[error("vector store: {0}")]
    Vector(#[from] moa_memory_vector::Error),
    /// JSON serialization for audit payload hashing failed.
    #[error("json serialization: {0}")]
    Json(#[from] serde_json::Error),
}
