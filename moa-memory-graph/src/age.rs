//! Apache AGE-backed `GraphStore` implementation.

use std::sync::Arc;

use moa_core::{ScopeContext, ScopedConn};
use moa_memory_vector::VectorStore;
use sqlx::{PgConnection, PgPool};

use crate::{GraphError, NodeWriteIntent};

/// Graph store backed by Apache AGE plus SQL sidecar tables.
#[derive(Clone)]
pub struct AgeGraphStore {
    pub(crate) pool: PgPool,
    pub(crate) scope: Option<ScopeContext>,
    pub(crate) assume_app_role: bool,
    pub(crate) vector: Option<Arc<dyn VectorStore>>,
}

impl AgeGraphStore {
    /// Creates an AGE graph store using the provided Postgres pool.
    ///
    /// This constructor does not install request-scope GUCs. Use `scoped` for tenant-context
    /// application paths.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            scope: None,
            assume_app_role: false,
            vector: None,
        }
    }

    /// Creates an AGE graph store that installs scope GUCs for each operation.
    pub fn scoped(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
            vector: None,
        }
    }

    /// Creates a scoped graph store that assumes `moa_app` inside each transaction.
    ///
    /// This is intended for integration tests that connect as `moa_owner` while still exercising
    /// application RLS policies.
    pub fn scoped_for_app_role(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: true,
            vector: None,
        }
    }

    /// Attaches a vector backend used by graph write operations.
    pub fn with_vector_store(mut self, vector: Arc<dyn VectorStore>) -> Self {
        self.vector = Some(vector);
        self
    }

    /// Returns the underlying Postgres pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the request scope installed before graph operations, when configured.
    pub fn scope(&self) -> Option<&ScopeContext> {
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
    ) -> Result<uuid::Uuid, GraphError> {
        crate::write::create_node_in_conn(self, conn, intent).await
    }

    pub(crate) async fn begin(&self) -> Result<Option<ScopedConn<'_>>, GraphError> {
        let Some(scope) = &self.scope else {
            return Ok(None);
        };

        let mut conn = ScopedConn::begin(&self.pool, scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        Ok(Some(conn))
    }

    pub(crate) async fn begin_required(&self) -> Result<ScopedConn<'_>, GraphError> {
        self.begin().await?.ok_or(GraphError::MissingScope)
    }
}
