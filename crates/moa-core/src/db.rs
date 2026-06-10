//! Postgres helpers shared by MOA storage crates.

use std::time::Instant;

use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{
    MoaError, Result, ScopeContext, record_scoped_guc_application_duration,
    record_scoped_transaction_begin_duration,
};

/// Transaction wrapper that installs MOA row-level-security GUCs before use.
pub struct ScopedConn<'p> {
    tx: Transaction<'p, Postgres>,
}

impl<'p> ScopedConn<'p> {
    /// Begins a transaction and applies the provided request scope to Postgres GUCs.
    pub async fn begin(pool: &'p PgPool, ctx: &ScopeContext) -> Result<Self> {
        let begin_started = Instant::now();
        let tx = pool.begin().await;
        record_scoped_transaction_begin_duration(begin_started.elapsed());
        let mut tx = tx.map_err(map_sqlx_error)?;

        let guc_started = Instant::now();
        let guc_result = Self::apply_gucs(&mut tx, ctx).await;
        record_scoped_guc_application_duration(guc_started.elapsed());
        guc_result?;

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

        sqlx::query(
            r#"
            SELECT
                pg_catalog.set_config('moa.workspace_id', $1, true),
                pg_catalog.set_config('moa.user_id', $2, true),
                pg_catalog.set_config('moa.scope_tier', $3, true),
                pg_catalog.set_config('search_path', 'ag_catalog, "$user", public', true)
            "#,
        )
        .bind(workspace)
        .bind(user)
        .bind(ctx.tier_str())
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
