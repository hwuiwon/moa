//! Graph-memory store, AGE templates, and SQL sidecar helpers.

use async_trait::async_trait;
use uuid::Uuid;

pub mod age;
pub mod changelog;
pub mod cypher;
pub mod edge;
pub mod error;
pub mod lexical;
pub mod node;
pub mod read;
pub mod write;

pub use age::AgeGraphStore;
pub use changelog::{ChangelogRecord, write_and_bump};
pub use edge::{EdgeLabel, EdgeWriteIntent};
pub use error::GraphError;
pub use lexical::LexicalStore;
pub use node::{
    NodeIndexRow, NodeLabel, NodeWriteIntent, PiiClass, bump_last_accessed, lookup_seed_by_name,
};

/// Result type returned by graph-memory helpers.
pub type Result<T> = std::result::Result<T, GraphError>;

/// Canonical graph-memory storage interface.
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

    /// Hard-purges a node from AGE and sidecar tables, preserving an erase changelog marker.
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
    ) -> Result<Vec<NodeIndexRow>>;

    /// Looks up NER seed nodes by name through the sidecar full-text index.
    async fn lookup_seeds(&self, name: &str, limit: i64) -> Result<Vec<NodeIndexRow>>;
}
