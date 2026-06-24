//! Database helpers shared by MOA storage crates.

use std::time::{Duration, Instant};

use moa_core::{ContactId, MoaError, Result, TenantId};
use moa_memory_types::ScopeContext;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

struct DbScopeGucs {
    tenant_id: Option<String>,
    contact_id: Option<String>,
    control_plane: bool,
}

fn record_scoped_transaction_begin_duration(duration: Duration) {
    metrics::histogram!("moa_scoped_transaction_begin_seconds").record(duration.as_secs_f64());
}

fn record_scoped_guc_application_duration(duration: Duration) {
    metrics::histogram!("moa_scoped_guc_application_seconds").record(duration.as_secs_f64());
}

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

    /// Begins a tenant-scoped transaction without contact access.
    pub async fn begin_tenant(pool: &'p PgPool, tenant_id: TenantId) -> Result<Self> {
        Self::begin(pool, &ScopeContext::tenant(tenant_id)).await
    }

    /// Begins a contact-scoped transaction inside one tenant.
    pub async fn begin_contact(
        pool: &'p PgPool,
        tenant_id: TenantId,
        contact_id: ContactId,
    ) -> Result<Self> {
        Self::begin(pool, &ScopeContext::contact(tenant_id, contact_id)).await
    }

    /// Begins an explicit workspace control-plane transaction.
    pub async fn begin_control_plane(pool: &'p PgPool) -> Result<Self> {
        let begin_started = Instant::now();
        let tx = pool.begin().await;
        record_scoped_transaction_begin_duration(begin_started.elapsed());
        let mut tx = tx.map_err(map_sqlx_error)?;

        let guc_started = Instant::now();
        let guc_result = Self::apply_guc_values(
            &mut tx,
            &DbScopeGucs {
                tenant_id: None,
                contact_id: None,
                control_plane: true,
            },
        )
        .await;
        record_scoped_guc_application_duration(guc_started.elapsed());
        guc_result?;

        Ok(Self { tx })
    }

    /// Applies MOA scope GUCs to an existing transaction.
    pub async fn apply_gucs(tx: &mut Transaction<'_, Postgres>, ctx: &ScopeContext) -> Result<()> {
        let contact_id = ctx
            .contact_id()
            .map(|contact_id| contact_id.to_string())
            .unwrap_or_default();
        Self::apply_guc_values(
            tx,
            &DbScopeGucs {
                tenant_id: Some(ctx.tenant_id().to_string()),
                contact_id: Some(contact_id),
                control_plane: false,
            },
        )
        .await
    }

    async fn apply_guc_values(
        tx: &mut Transaction<'_, Postgres>,
        gucs: &DbScopeGucs,
    ) -> Result<()> {
        sqlx::query(
            r#"
            SELECT
                pg_catalog.set_config('moa.tenant_id', $1, true),
                pg_catalog.set_config('moa.contact_id', $2, true),
                pg_catalog.set_config('moa.control_plane', $3, true),
                pg_catalog.set_config('search_path', 'ag_catalog, "$user", public', true)
            "#,
        )
        .bind(gucs.tenant_id.as_deref().unwrap_or(""))
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
