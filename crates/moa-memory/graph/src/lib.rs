//! Graph-memory store, relational graph tables, and SQL sidecar helpers.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

pub mod changelog;
pub mod edge;
pub mod error;
pub mod lexical;
pub mod node;
pub mod read;
pub mod store;
pub mod validity;
pub mod write;

pub use changelog::{ChangelogRecord, write_and_bump};
pub use edge::{EdgeLabel, EdgeWriteIntent};
pub use error::GraphError;
pub use lexical::LexicalStore;
pub use node::{
    ExistingSupersessionIntent, NodeEmbeddingIntent, NodeIndexRow, NodeLabel,
    NodePropertyUpdateIntent, NodeWriteIntent, PiiClass, bump_last_accessed, lookup_seed_by_name,
};
pub use store::PostgresGraphStore;
pub use validity::push_validity_filter;
pub use write::{
    close_existing_node_with_supersession, update_node_properties, upsert_node_embedding,
};

/// Result type returned by graph-memory helpers.
pub type Result<T> = std::result::Result<T, GraphError>;

/// One path discovered while expanding graph retrieval seeds.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphExpansionHit {
    /// Candidate node reached by traversal.
    pub uid: Uuid,
    /// Candidate node label from the sidecar projection.
    pub label: NodeLabel,
    /// Input seed that reached this candidate.
    pub seed: Uuid,
    /// Validity start for the input seed row used by this path.
    pub seed_valid_from: DateTime<Utc>,
    /// Validity start for the reached candidate row.
    pub valid_from: DateTime<Utc>,
    /// One-based distance from the seed.
    pub hop: u8,
    /// Edge labels along the shortest discovered path.
    pub edges: Vec<EdgeLabel>,
}

/// Canonical graph-memory persistence interface.
#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Creates a new node, sidecar projection, and changelog row in one transaction.
    async fn create_node(&self, intent: NodeWriteIntent) -> Result<Uuid>;

    /// Creates a new node using a caller-owned scoped Postgres connection.
    ///
    /// Callers use this when the graph write must be composed with adjacent SQL writes in one
    /// outer transaction.
    async fn create_node_in_conn(
        &self,
        _conn: &mut sqlx::PgConnection,
        _intent: NodeWriteIntent,
    ) -> Result<Uuid> {
        Err(GraphError::Conflict(
            "caller-owned graph writes are not supported by this store".to_string(),
        ))
    }

    /// Updates properties on an existing node by superseding it with a new node.
    async fn supersede_node(&self, old_uid: Uuid, intent: NodeWriteIntent) -> Result<Uuid>;

    /// Soft-invalidates a node by setting its validity end and invalidation metadata.
    async fn invalidate_node(&self, uid: Uuid, reason: &str) -> Result<()>;

    /// Hard-purges a node from graph tables, preserving an erase changelog marker.
    async fn hard_purge(&self, uid: Uuid, redaction_marker: &str) -> Result<()>;

    /// Creates an edge between two nodes.
    async fn create_edge(&self, intent: EdgeWriteIntent) -> Result<Uuid>;

    /// Looks up a single node by stable uid.
    async fn get_node(&self, uid: Uuid) -> Result<Option<NodeIndexRow>>;

    /// Traverses one to three hops from a seed node and returns sidecar rows for visible nodes.
    async fn neighbors(
        &self,
        seed: Uuid,
        hops: u8,
        edge_filter: Option<&[EdgeLabel]>,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>>;

    /// Expands a batch of seed nodes and returns bounded shortest labeled paths to visible nodes.
    async fn expand_seeds(
        &self,
        seeds: &[Uuid],
        max_hops: u8,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<GraphExpansionHit>>;

    /// Looks up NER seed nodes by name through the sidecar full-text index.
    async fn lookup_seeds(
        &self,
        name: &str,
        limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<NodeIndexRow>>;
}
