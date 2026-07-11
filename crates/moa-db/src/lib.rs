//! Database helpers shared by MOA storage crates.

use std::time::Instant;

use moa_core::{
    error::MoaError, error::Result, types::contact::ContactId, types::identifiers::TenantId,
    types::memory::RlsContext,
};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

struct DbScopeGucs {
    tenant_id: Option<String>,
    storage_partition_id: Option<String>,
    contact_id: Option<String>,
    control_plane: bool,
}

/// Transaction wrapper that installs MOA row-level-security GUCs before use.
pub struct ScopedConn<'p> {
    tx: Transaction<'p, Postgres>,
}

impl<'p> ScopedConn<'p> {
    /// Begins a transaction and applies the provided request scope to Postgres GUCs.
    pub async fn begin(pool: &'p PgPool, ctx: &RlsContext) -> Result<Self> {
        Self::begin_with_gucs(pool, &Self::scope_gucs(ctx)).await
    }

    /// Begins a transaction and optionally promotes it to the `moa_app` role.
    ///
    /// When `assume_app_role` is true the transaction switches to the `moa_app`
    /// role via `SET LOCAL ROLE` after the request scope GUCs are applied,
    /// matching the pattern used by row-level-security protected writers.
    pub async fn begin_as_app(
        pool: &'p PgPool,
        ctx: &RlsContext,
        assume_app_role: bool,
    ) -> Result<Self> {
        let mut conn = Self::begin(pool, ctx).await?;
        if assume_app_role {
            conn.assume_app_role().await?;
        }
        Ok(conn)
    }

    /// Begins a tenant-scoped transaction without contact access.
    pub async fn begin_tenant(pool: &'p PgPool, tenant_id: TenantId) -> Result<Self> {
        Self::begin(pool, &RlsContext::tenant(tenant_id)).await
    }

    /// Begins a contact-scoped transaction inside one tenant.
    pub async fn begin_contact(
        pool: &'p PgPool,
        tenant_id: TenantId,
        contact_id: ContactId,
    ) -> Result<Self> {
        Self::begin(pool, &RlsContext::contact(tenant_id, contact_id)).await
    }

    /// Begins an explicit tenant control-plane transaction.
    pub async fn begin_control_plane(pool: &'p PgPool) -> Result<Self> {
        Self::begin_with_gucs(
            pool,
            &DbScopeGucs {
                tenant_id: None,
                storage_partition_id: None,
                contact_id: None,
                control_plane: true,
            },
        )
        .await
    }

    /// Promotes the current transaction to the `moa_app` role for its remainder.
    ///
    /// Required before touching row-level-security protected tables as the MOA
    /// application role.
    pub async fn assume_app_role(&mut self) -> Result<()> {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(self.as_mut())
            .await
            .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Begins a transaction and applies the provided GUC scope, recording timing.
    async fn begin_with_gucs(pool: &'p PgPool, gucs: &DbScopeGucs) -> Result<Self> {
        let begin_started = Instant::now();
        let tx = pool.begin().await;
        metrics::histogram!("moa_scoped_transaction_begin_seconds")
            .record(begin_started.elapsed().as_secs_f64());
        let mut tx = tx.map_err(map_sqlx_error)?;

        let guc_started = Instant::now();
        let guc_result = Self::apply_guc_values(&mut tx, gucs).await;
        metrics::histogram!("moa_scoped_guc_application_seconds")
            .record(guc_started.elapsed().as_secs_f64());
        guc_result?;

        Ok(Self { tx })
    }

    /// Applies MOA scope GUCs to an existing transaction.
    pub async fn apply_gucs(tx: &mut Transaction<'_, Postgres>, ctx: &RlsContext) -> Result<()> {
        Self::apply_guc_values(tx, &Self::scope_gucs(ctx)).await
    }

    /// Builds the request-scope GUC values from an [`RlsContext`].
    fn scope_gucs(ctx: &RlsContext) -> DbScopeGucs {
        DbScopeGucs {
            storage_partition_id: Some(ctx.storage_partition_id().to_string()),
            tenant_id: Some(ctx.tenant_id().to_string()),
            contact_id: Some(
                ctx.contact_id()
                    .map(|contact_id| contact_id.to_string())
                    .unwrap_or_default(),
            ),
            control_plane: false,
        }
    }

    async fn apply_guc_values(
        tx: &mut Transaction<'_, Postgres>,
        gucs: &DbScopeGucs,
    ) -> Result<()> {
        sqlx::query(
            r#"
            SELECT
                pg_catalog.set_config('moa.tenant_id', $1, true),
                pg_catalog.set_config('moa.storage_partition_id', $2, true),
                pg_catalog.set_config('moa.contact_id', $3, true),
                pg_catalog.set_config('moa.control_plane', $4, true)
            "#,
        )
        .bind(gucs.tenant_id.as_deref().unwrap_or(""))
        .bind(gucs.storage_partition_id.as_deref().unwrap_or(""))
        .bind(gucs.contact_id.as_deref().unwrap_or(""))
        .bind(if gucs.control_plane { "true" } else { "false" })
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
