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
    lookup_seeds_by_names,
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

    /// Looks up several nodes by stable uid in one round trip.
    ///
    /// Returns the rows that exist (missing uids are omitted, mirroring
    /// [`Self::get_node`] returning `None`); the result order is unspecified, so
    /// callers that need a uid-keyed view build a map from it. The default
    /// implementation loops [`Self::get_node`]; stores backed by a batchable
    /// query override it to replace an N+1 lookup with a single query.
    async fn bulk_get_nodes(&self, uids: &[Uuid]) -> Result<Vec<NodeIndexRow>> {
        let mut rows = Vec::with_capacity(uids.len());
        for uid in uids {
            if let Some(row) = self.get_node(*uid).await? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Creates several nodes, sidecar rows, optional vectors, and changelog rows
    /// in one transaction.
    ///
    /// Returns the created uids in input order. The default implementation loops
    /// [`Self::create_node`] (one transaction per node); stores backed by a
    /// batchable write override it to insert every `node_index` row in a single
    /// statement while still writing one `graph_changelog` outbox row per node.
    /// Callers use this to replace an N+1 create loop.
    async fn bulk_create_nodes(&self, intents: Vec<NodeWriteIntent>) -> Result<Vec<Uuid>> {
        let mut uids = Vec::with_capacity(intents.len());
        for intent in intents {
            uids.push(self.create_node(intent).await?);
        }
        Ok(uids)
    }

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

    /// Looks up NER seed nodes for several names, returning one result set per name.
    ///
    /// Results are returned in the same order as `names`; each entry is the seed
    /// set [`Self::lookup_seeds`] would return for that name. Callers use this to
    /// replace an N+1 loop of per-name lookups (for example one query planner
    /// span per scope) with a single batched query. The default implementation
    /// loops [`Self::lookup_seeds`]; stores backed by a batchable query override
    /// it.
    async fn lookup_seeds_batch(
        &self,
        names: &[&str],
        limit: i64,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<Vec<NodeIndexRow>>> {
        let mut results = Vec::with_capacity(names.len());
        for name in names {
            results.push(self.lookup_seeds(name, limit, as_of).await?);
        }
        Ok(results)
    }
}
