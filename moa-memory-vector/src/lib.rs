//! Vector storage and embedding abstractions for graph memory.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod embedder;
pub mod pgvector_store;

pub use embedder::{CohereV4Embedder, Embedder};
pub use pgvector_store::PgvectorStore;

/// Fixed graph-memory embedding dimensionality.
pub const VECTOR_DIMENSION: usize = 1024;

/// Result type returned by vector-memory helpers.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by vector-memory helpers.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An embedding had the wrong dimensionality.
    #[error("embedding dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch {
        /// Expected number of dimensions.
        expected: usize,
        /// Actual number of dimensions.
        actual: usize,
    },
    /// A PII class string is not part of the supported hierarchy.
    #[error("unknown PII class `{0}`")]
    UnknownPiiClass(String),
    /// The embedding response count did not match the input count.
    #[error("embedding response length mismatch: expected {expected}, got {actual}")]
    EmbeddingResponseLength {
        /// Expected number of embeddings.
        expected: usize,
        /// Actual number of embeddings.
        actual: usize,
    },
    /// The embedding provider returned a non-success status.
    #[error("embedding provider returned HTTP {status}: {body}")]
    ProviderStatus {
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },
    /// The configured query limit is too large for Postgres.
    #[error("vector query limit {0} does not fit into i64")]
    QueryLimitTooLarge(usize),
    /// A core storage helper failed.
    #[error("core storage helper failed: {0}")]
    Core(#[from] moa_core::MoaError),
    /// A Postgres query failed.
    #[error("vector store query failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// An HTTP request failed.
    #[error("embedding HTTP request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
}

/// One vector row to upsert into the vector store.
#[derive(Debug, Clone)]
pub struct VectorItem {
    /// Stable graph node identity.
    pub uid: Uuid,
    /// Workspace owner for workspace and user scoped rows.
    pub workspace_id: Option<String>,
    /// User owner for user scoped rows.
    pub user_id: Option<String>,
    /// Graph vertex label.
    pub label: String,
    /// PII class used by retrieval filters.
    pub pii_class: String,
    /// Dense 1024-dimensional embedding.
    pub embedding: Vec<f32>,
    /// Embedding model identifier.
    pub embedding_model: String,
    /// Embedding model version for dual-write upgrades.
    pub embedding_model_version: i32,
    /// End of validity for soft-deleted or superseded embeddings.
    pub valid_to: Option<DateTime<Utc>>,
}

/// KNN vector query parameters.
#[derive(Debug, Clone)]
pub struct VectorQuery {
    /// Dense 1024-dimensional query embedding.
    pub embedding: Vec<f32>,
    /// Number of nearest neighbors to return.
    pub k: usize,
    /// Optional graph label allowlist.
    pub label_filter: Option<Vec<String>>,
    /// Maximum allowed PII class using the hierarchy `none < pii < phi < restricted`.
    pub max_pii_class: String,
    /// Whether global rows should remain eligible after RLS has scoped visibility.
    pub include_global: bool,
}

/// One KNN result from vector retrieval.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorMatch {
    /// Matched graph node identity.
    pub uid: Uuid,
    /// Cosine similarity score where 1.0 is identical.
    pub score: f32,
}

/// Storage abstraction implemented by pgvector and future vector backends.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Returns the backend identifier.
    fn backend(&self) -> &'static str;

    /// Returns the fixed embedding dimensionality accepted by this store.
    fn dimension(&self) -> usize;

    /// Inserts or updates embeddings in the current store scope.
    async fn upsert(&self, items: &[VectorItem]) -> Result<()>;

    /// Runs a scoped nearest-neighbor query.
    async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>>;

    /// Deletes embeddings in the current store scope by node id.
    async fn delete(&self, uids: &[Uuid]) -> Result<()>;
}

pub(crate) fn validate_dimension(embedding: &[f32]) -> Result<()> {
    if embedding.len() == VECTOR_DIMENSION {
        Ok(())
    } else {
        Err(Error::DimensionMismatch {
            expected: VECTOR_DIMENSION,
            actual: embedding.len(),
        })
    }
}

pub(crate) fn pii_rank(value: &str) -> Result<i32> {
    match value {
        "none" => Ok(0),
        "pii" => Ok(1),
        "phi" => Ok(2),
        "restricted" => Ok(3),
        other => Err(Error::UnknownPiiClass(other.to_string())),
    }
}
