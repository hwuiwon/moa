//! Vector storage and embedding abstractions for graph memory.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

pub mod backend;
pub mod pgvector_store;
pub mod promotion;
pub mod turbopuffer;

pub use backend::vector_store_for_storage_partition;
pub use pgvector_store::PgvectorStore;
pub use promotion::{
    PROMOTION_BATCH_SIZE, PROMOTION_OVERLAP_THRESHOLD, PromotionOptions, PromotionReport,
    VectorPartitionPromotion, finalize_promotion, rollback_promotion,
};
pub use turbopuffer::TurbopufferStore;

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
    /// Embedder configuration is invalid.
    #[error("invalid embedder configuration: {0}")]
    EmbedderConfig(String),
    /// The storage-partition embedder configuration does not match the pgvector index shape.
    #[error(
        "storage partition {storage_partition_id} embedder `{configured_model}` uses {configured_dimension} dimensions, but pgvector KNN requires {required_dimension}"
    )]
    EmbedderMismatch {
        /// Storage partition with the mismatched embedder.
        storage_partition_id: String,
        /// Configured embedder model.
        configured_model: String,
        /// Configured embedding dimensionality.
        configured_dimension: usize,
        /// Dimensionality required by the active vector index.
        required_dimension: usize,
    },
    /// The storage partition has no persisted embedder state row.
    #[error("storage partition {storage_partition_id} has no configured embedder state")]
    StoragePartitionEmbedderStateMissing {
        /// Storage partition missing embedder state.
        storage_partition_id: String,
    },
    /// A write attempted to mix embedding models inside one storage partition vector space.
    #[error(
        "storage partition {storage_partition_id} embedder `{configured_model}` cannot accept `{requested_model}` vectors"
    )]
    EmbedderModelMismatch {
        /// Storage partition with the mismatched embedder model.
        storage_partition_id: String,
        /// Configured embedder model.
        configured_model: String,
        /// Model used by the embedding write.
        requested_model: String,
    },
    /// The storage partition is being re-embedded and cannot serve stale KNN reads.
    #[error("storage partition {storage_partition_id} re-embedding is in progress")]
    ReembedInProgress {
        /// Storage partition whose vectors are being rewritten.
        storage_partition_id: String,
    },
    /// The vector provider returned a non-success status.
    #[error("vector provider `{provider}` returned HTTP {status}: {body}")]
    VectorProviderStatus {
        /// Vector backend identifier.
        provider: &'static str,
        /// HTTP status code.
        status: u16,
        /// Response body text.
        body: String,
    },
    /// The configured query limit is too large for Postgres.
    #[error("vector query limit {0} does not fit into i64")]
    QueryLimitTooLarge(usize),
    /// The vector backend needs a scoped storage partition.
    #[error("vector backend `{backend}` requires a scoped storage partition for {operation}")]
    StoragePartitionRequired {
        /// Vector backend identifier.
        backend: &'static str,
        /// Operation that requires a storage partition.
        operation: &'static str,
    },
    /// The requested vector backend is not configured.
    #[error(
        "storage partition {storage_partition_id} is configured for turbopuffer, but no client is configured"
    )]
    TurbopufferUnavailable {
        /// Storage partition that requested Turbopuffer.
        storage_partition_id: String,
    },
    /// A HIPAA storage partition requested Turbopuffer without a BAA-enabled client.
    #[error(
        "storage partition {storage_partition_id} is HIPAA-tier and requires a Turbopuffer BAA"
    )]
    TurbopufferBaaRequired {
        /// Storage partition that requested Turbopuffer.
        storage_partition_id: String,
    },
    /// Turbopuffer returned a malformed response.
    #[error("invalid turbopuffer response: {0}")]
    TurbopufferResponse(String),
    /// Turbopuffer configuration is invalid.
    #[error("invalid turbopuffer configuration: {0}")]
    TurbopufferConfig(String),
    /// Vector partition promotion validation failed.
    #[error(
        "vector partition promotion validation failed: overlap {overlap:.3} below {required:.3}"
    )]
    PromotionValidationFailed {
        /// Observed top-K overlap.
        overlap: f64,
        /// Required top-K overlap.
        required: f64,
    },
    /// Vector partition promotion state does not allow the requested operation.
    #[error("vector partition promotion state `{state}` does not allow {operation}")]
    InvalidPromotionState {
        /// Current promotion state.
        state: String,
        /// Operation being attempted.
        operation: &'static str,
    },
    /// A core storage helper failed.
    #[error("core storage helper failed: {0}")]
    Core(#[from] moa_core::MoaError),
    /// A Postgres query failed.
    #[error("vector store query failed: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// The vector backend cannot participate in the caller's Postgres transaction.
    #[error("vector backend `{0}` does not support Postgres transactional writes")]
    TransactionalWritesUnsupported(&'static str),
    /// The vector backend cannot satisfy a requested query feature.
    #[error("vector backend `{backend}` does not support query feature `{feature}`")]
    UnsupportedQueryFeature {
        /// Vector backend identifier.
        backend: &'static str,
        /// Unsupported feature identifier.
        feature: &'static str,
    },
    /// An HTTP request failed.
    #[error("embedding HTTP request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    /// JSON serialization or deserialization failed.
    #[error("vector JSON serialization failed: {0}")]
    SerdeJson(#[from] serde_json::Error),
}

impl From<Error> for moa_core::MoaError {
    fn from(error: Error) -> Self {
        match error {
            Error::Core(error) => error,
            Error::ProviderStatus { status, body } => Self::HttpStatus {
                status,
                retry_after: None,
                message: body,
            },
            Error::VectorProviderStatus { status, body, .. } => Self::HttpStatus {
                status,
                retry_after: None,
                message: body,
            },
            Error::Sqlx(error) => Self::StorageError(error.to_string()),
            Error::Reqwest(error) => Self::ProviderError(error.to_string()),
            Error::SerdeJson(error) => Self::SerializationError(error.to_string()),
            other => Self::ProviderError(other.to_string()),
        }
    }
}

/// One vector row to upsert into the vector store.
#[derive(Debug, Clone)]
pub struct VectorItem {
    /// Stable graph node identity.
    pub uid: Uuid,
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
    /// Optional application-time filter for bitemporal retrieval.
    pub as_of: Option<DateTime<Utc>>,
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

    /// Inserts or updates embeddings using the caller's scoped Postgres transaction connection.
    async fn upsert_in_tx(&self, conn: &mut PgConnection, items: &[VectorItem]) -> Result<()> {
        let _ = conn;
        let _ = items;
        Err(Error::TransactionalWritesUnsupported(self.backend()))
    }

    /// Runs a scoped nearest-neighbor query.
    async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>>;

    /// Deletes embeddings in the current store scope by node id.
    async fn delete(&self, uids: &[Uuid]) -> Result<()>;

    /// Deletes embeddings using the caller's scoped Postgres transaction connection.
    async fn delete_in_tx(&self, conn: &mut PgConnection, uids: &[Uuid]) -> Result<()> {
        let _ = conn;
        let _ = uids;
        Err(Error::TransactionalWritesUnsupported(self.backend()))
    }
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
