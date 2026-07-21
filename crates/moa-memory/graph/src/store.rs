//! Relational Postgres-backed `GraphStore` implementation.

use std::sync::Arc;

use moa_core::types::memory::RlsContext;
use moa_crypto::KeyManagementProvider;
use moa_db::ScopedConn;
use moa_memory_vector::VectorStore;
use sqlx::{PgConnection, PgPool};

use crate::{
    Error, ExistingSupersessionIntent, NodeContentUpdateIntent, NodeEmbeddingIntent,
    NodeExpiryIntent, NodeWriteIntent,
};

/// Graph store backed by relational node, edge, changelog, and vector tables.
///
/// The key-management provider seals restricted/PHI `name` and
/// `properties_summary` on the write path and opens them on the read path (see
/// [`crate::write`] and [`crate::read`]). Every constructor requires the shared
/// provider explicitly so sealed content is readable across store instances and
/// process restarts.
#[derive(Clone)]
pub struct PostgresGraphStore {
    pub(crate) pool: PgPool,
    pub(crate) scope: Option<RlsContext>,
    pub(crate) assume_app_role: bool,
    pub(crate) vector: Option<Arc<dyn VectorStore>>,
    pub(crate) kms: Arc<dyn KeyManagementProvider>,
}

impl PostgresGraphStore {
    /// Creates a graph store using the provided Postgres pool.
    ///
    /// This constructor does not install request-scope GUCs. Use `scoped` for tenant-context
    /// application paths.
    pub fn new(pool: PgPool, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            scope: None,
            assume_app_role: false,
            vector: None,
            kms,
        }
    }

    /// Creates a graph store that installs scope GUCs for each operation.
    pub fn scoped(pool: PgPool, scope: RlsContext, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
            vector: None,
            kms,
        }
    }

    /// Creates a scoped graph store that assumes `moa_app` inside each transaction.
    ///
    /// This is intended for integration tests that connect as `moa_owner` while still exercising
    /// application RLS policies.
    pub fn scoped_for_app_role(
        pool: PgPool,
        scope: RlsContext,
        kms: Arc<dyn KeyManagementProvider>,
    ) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: true,
            vector: None,
            kms,
        }
    }

    /// Attaches a vector backend used by graph write operations.
    pub fn with_vector_store(mut self, vector: Arc<dyn VectorStore>) -> Self {
        self.vector = Some(vector);
        self
    }

    /// Returns the key-management provider used for restricted node content.
    pub fn kms(&self) -> &Arc<dyn KeyManagementProvider> {
        &self.kms
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

    /// Creates a node using a caller-owned scoped Postgres connection.
    ///
    /// This is used by adjacent domain crates that need to compose a graph write with their own
    /// table updates in one transaction.
    pub async fn create_node_in_conn(
        &self,
        conn: &mut PgConnection,
        intent: NodeWriteIntent,
    ) -> Result<uuid::Uuid, Error> {
        crate::write::create_node_in_conn(self, conn, intent).await
    }

    /// Closes an active node into an already-existing replacement node.
    pub async fn close_existing_node_with_supersession(
        &self,
        intent: ExistingSupersessionIntent,
    ) -> Result<(), Error> {
        crate::write::close_existing_node_with_supersession(self, intent).await
    }

    /// Closes an active node without a replacement, at caller-provided instants.
    pub async fn expire_node(&self, intent: NodeExpiryIntent) -> Result<bool, Error> {
        crate::write::expire_node(self, intent).await
    }

    /// Replaces a node's complete mutable content in place.
    pub async fn update_node_content(&self, intent: NodeContentUpdateIntent) -> Result<(), Error> {
        crate::write::update_node_content(self, intent).await
    }

    /// Attaches a vector embedding to an existing node.
    pub async fn upsert_node_embedding(&self, intent: NodeEmbeddingIntent) -> Result<(), Error> {
        crate::write::upsert_node_embedding(self, intent).await
    }

    /// Begins a scoped transaction when this store has an RLS context.
    pub(crate) async fn begin(&self) -> Result<Option<ScopedConn<'_>>, Error> {
        let Some(scope) = &self.scope else {
            return Ok(None);
        };

        let conn = ScopedConn::begin_as_app(&self.pool, scope, self.assume_app_role).await?;
        Ok(Some(conn))
    }

    /// Begins a scoped transaction and fails when this store has no RLS context.
    pub(crate) async fn begin_required(&self) -> Result<ScopedConn<'_>, Error> {
        self.begin().await?.ok_or(Error::MissingScope)
    }
}
