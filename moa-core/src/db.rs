//! Postgres helpers shared by MOA storage crates.

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{MoaError, Result, ScopeContext};

/// Transaction wrapper that installs MOA row-level-security GUCs before use.
pub struct ScopedConn<'p> {
    tx: Transaction<'p, Postgres>,
}

impl<'p> ScopedConn<'p> {
    /// Begins a transaction and applies the provided request scope to Postgres GUCs.
    pub async fn begin(pool: &'p PgPool, ctx: &ScopeContext) -> Result<Self> {
        let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
        Self::apply_gucs(&mut tx, ctx).await?;
        Ok(Self { tx })
    }

    /// Applies MOA scope GUCs to an existing transaction.
    pub async fn apply_gucs(tx: &mut Transaction<'_, Postgres>, ctx: &ScopeContext) -> Result<()> {
        let workspace = ctx
            .workspace_id()
            .map(|workspace_id| workspace_id.to_string())
            .unwrap_or_default();
        let user = ctx
            .user_id()
            .map(|user_id| user_id.to_string())
            .unwrap_or_default();

        sqlx::query("SELECT pg_catalog.set_config('moa.workspace_id', $1, true)")
            .bind(workspace)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query("SELECT pg_catalog.set_config('moa.user_id', $1, true)")
            .bind(user)
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query("SELECT pg_catalog.set_config('moa.scope_tier', $1, true)")
            .bind(ctx.tier_str())
            .execute(&mut **tx)
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(
            "SELECT pg_catalog.set_config('search_path', 'ag_catalog, \"$user\", public', true)",
        )
        .execute(&mut **tx)
        .await
        .map_err(map_sqlx_error)?;

        Ok(())
    }

    /// Commits the scoped transaction.
    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await.map_err(map_sqlx_error)
    }

    /// Rolls back the scoped transaction.
    pub async fn rollback(self) -> Result<()> {
        self.tx.rollback().await.map_err(map_sqlx_error)
    }
}

impl AsMut<PgConnection> for ScopedConn<'_> {
    fn as_mut(&mut self) -> &mut PgConnection {
        &mut self.tx
    }
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
