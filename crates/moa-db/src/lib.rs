//! Database helpers shared by MOA storage crates.

use moa_core::{
    error::MoaError,
    error::Result,
    types::contact::ContactId,
    types::identifiers::TenantId,
    types::memory::{InformationBarrierClearances, RlsContext},
};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

pub mod source_acl;

pub use source_acl::{
    MAX_SOURCE_ACL_PRINCIPALS, TENANT_WIDE_PRINCIPAL_HOLDER, current_source_acl_epoch,
    push_source_acl_predicate, resolve_source_acl_context,
};

struct DbScopeGucs {
    tenant_id: Option<String>,
    storage_partition_id: Option<String>,
    contact_id: Option<String>,
    /// Comma-delimited cleared information-barrier tags for the need-to-know
    /// read policy. `None`/empty installs an empty clearance (fail closed).
    cleared_barriers: Option<String>,
    control_plane: bool,
}

/// Serializes cleared information-barrier tags into the comma-delimited form the
/// `moa.cleared_barriers` GUC and the `rd_barrier_need_to_know` RLS policy parse.
///
/// A comma is the list delimiter, so any tag containing one is DROPPED rather
/// than silently split into spurious clearances. Dropping is fail-closed: the
/// caller is simply not treated as cleared for that malformed tag, which can
/// only hide barriered rows, never leak them.
fn join_cleared_barriers(cleared: &InformationBarrierClearances) -> String {
    cleared
        .iter()
        .map(|tag| tag.as_str())
        .collect::<Vec<_>>()
        .join(",")
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
                cleared_barriers: None,
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

    /// Replaces the current request scope with one contact-local scope and
    /// assumes the `moa_app` role on the same transaction connection.
    ///
    /// The full RLS context is reinstalled, including tenant, storage
    /// partition, contact, control-plane state, and information-barrier
    /// clearances, so no value from an earlier scope survives the transition.
    pub async fn assume_app_contact_scope(
        &mut self,
        tenant_id: TenantId,
        contact_id: ContactId,
    ) -> Result<()> {
        let gucs = Self::scope_gucs(&RlsContext::contact(tenant_id, contact_id));
        Self::apply_guc_values(&mut self.tx, &gucs).await?;
        self.assume_app_role().await
    }

    /// Begins a transaction and applies the provided GUC scope.
    async fn begin_with_gucs(pool: &'p PgPool, gucs: &DbScopeGucs) -> Result<Self> {
        let mut tx = pool.begin().await.map_err(map_sqlx_error)?;
        Self::apply_guc_values(&mut tx, gucs).await?;
        Ok(Self { tx })
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
            cleared_barriers: Some(join_cleared_barriers(ctx.cleared_barriers())),
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
                pg_catalog.set_config('moa.control_plane', $4, true),
                pg_catalog.set_config('moa.cleared_barriers', $5, true)
            "#,
        )
        .bind(gucs.tenant_id.as_deref().unwrap_or(""))
        .bind(gucs.storage_partition_id.as_deref().unwrap_or(""))
        .bind(gucs.contact_id.as_deref().unwrap_or(""))
        .bind(if gucs.control_plane { "true" } else { "false" })
        .bind(gucs.cleared_barriers.as_deref().unwrap_or(""))
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

pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
