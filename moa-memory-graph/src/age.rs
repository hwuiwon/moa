//! Apache AGE-backed `GraphStore` implementation.

use moa_core::{ScopeContext, ScopedConn};
use sqlx::PgPool;

use crate::GraphError;

/// Graph store backed by Apache AGE plus SQL sidecar tables.
#[derive(Clone)]
pub struct AgeGraphStore {
    pub(crate) pool: PgPool,
    pub(crate) scope: Option<ScopeContext>,
    pub(crate) assume_app_role: bool,
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
        }
    }

    /// Creates an AGE graph store that installs scope GUCs for each operation.
    pub fn scoped(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
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
        }
    }

    /// Returns the underlying Postgres pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the request scope installed before graph operations, when configured.
    pub fn scope(&self) -> Option<&ScopeContext> {
        self.scope.as_ref()
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
}
