//! Relational Postgres-backed `GraphStore` implementation.

use std::sync::Arc;

use moa_core::RlsContext;
use moa_db::ScopedConn;
use moa_memory_vector::{VectorPostCommitSync, VectorStore};
use sqlx::{PgConnection, PgPool};

use crate::{
    ExistingSupersessionIntent, GraphError, NodeEmbeddingIntent, NodePropertyUpdateIntent,
    NodeWriteIntent,
};

/// Graph store backed by relational node, edge, changelog, and vector tables.
#[derive(Clone)]
pub struct PostgresGraphStore {
    pub(crate) pool: PgPool,
    pub(crate) scope: Option<RlsContext>,
    pub(crate) assume_app_role: bool,
    pub(crate) vector: Option<Arc<dyn VectorStore>>,
    pub(crate) vector_post_commit_sync: Option<Arc<dyn VectorPostCommitSync>>,
}

impl PostgresGraphStore {
    /// Creates a graph store using the provided Postgres pool.
    ///
    /// This constructor does not install request-scope GUCs. Use `scoped` for tenant-context
    /// application paths.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            scope: None,
            assume_app_role: false,
            vector: None,
            vector_post_commit_sync: None,
        }
    }

    /// Creates a graph store that installs scope GUCs for each operation.
    pub fn scoped(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
            vector: None,
            vector_post_commit_sync: None,
        }
    }

    /// Creates a scoped graph store that assumes `moa_app` inside each transaction.
    ///
    /// This is intended for integration tests that connect as `moa_owner` while still exercising
    /// application RLS policies.
    pub fn scoped_for_app_role(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: true,
            vector: None,
            vector_post_commit_sync: None,
        }
    }

    /// Attaches a vector backend used by graph write operations.
    pub fn with_vector_store(mut self, vector: Arc<dyn VectorStore>) -> Self {
        self.vector = Some(vector);
        self
    }

    /// Attaches a post-commit hook for external vector projection sync.
    pub fn with_vector_post_commit_sync(mut self, hook: Arc<dyn VectorPostCommitSync>) -> Self {
        self.vector_post_commit_sync = Some(hook);
        self
    }

    /// Returns the underlying Postgres pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the request scope installed before graph operations, when configured.
    pub fn scope(&self) -> Option<&RlsContext> {
        self.scope.as_ref()
    }

    /// Returns the vector backend used by graph writes, when configured.
    pub fn vector(&self) -> Option<&dyn VectorStore> {
        self.vector.as_deref()
    }

    /// Returns the post-commit vector sync hook, when configured.
    pub fn vector_post_commit_sync(&self) -> Option<&dyn VectorPostCommitSync> {
        self.vector_post_commit_sync.as_deref()
    }

    /// Creates a node using a caller-owned scoped Postgres connection.
    ///
    /// This is used by adjacent domain crates that need to compose a graph write with their own
    /// table updates in one transaction.
    pub async fn create_node_in_conn(
        &self,
        conn: &mut PgConnection,
        intent: NodeWriteIntent,
    ) -> Result<uuid::Uuid, GraphError> {
        crate::write::create_node_in_conn(self, conn, intent).await
    }

    /// Closes an active node into an already-existing replacement node.
    pub async fn close_existing_node_with_supersession(
        &self,
        intent: ExistingSupersessionIntent,
    ) -> Result<(), GraphError> {
        crate::write::close_existing_node_with_supersession(self, intent).await
    }

    /// Updates a node's mutable properties in place.
    pub async fn update_node_properties(
        &self,
        intent: NodePropertyUpdateIntent,
    ) -> Result<(), GraphError> {
        crate::write::update_node_properties(self, intent).await
    }

    /// Attaches a vector embedding to an existing node.
    pub async fn upsert_node_embedding(
        &self,
        intent: NodeEmbeddingIntent,
    ) -> Result<(), GraphError> {
        crate::write::upsert_node_embedding(self, intent).await
    }

    /// Soft-invalidates one graph edge by closing its validity window.
    pub async fn invalidate_edge(&self, uid: uuid::Uuid, reason: &str) -> Result<(), GraphError> {
        crate::write::invalidate_edge(self, uid, reason).await
    }

    /// Begins a scoped transaction when this store has an RLS context.
    pub(crate) async fn begin(&self) -> Result<Option<ScopedConn<'_>>, GraphError> {
        let Some(scope) = &self.scope else {
            return Ok(None);
        };

        let conn = ScopedConn::begin_as_app(&self.pool, scope, self.assume_app_role).await?;
        Ok(Some(conn))
    }

    /// Begins a scoped transaction and fails when this store has no RLS context.
    pub(crate) async fn begin_required(&self) -> Result<ScopedConn<'_>, GraphError> {
        self.begin().await?.ok_or(GraphError::MissingScope)
    }
}
