//! Tenant administration persistence operations.

use restate_sdk::prelude::{HandlerError, TerminalError};
use uuid::Uuid;

use super::workspaces;

/// Ensure a tenant has an active OCSF signing key.
pub(crate) async fn ensure_signing_key(
    pool: sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<Uuid, HandlerError> {
    attach_tenant_to_workspace(&pool, tenant_id).await?;
    moa_ocsf::ensure_key(&pool, tenant_id)
        .await
        .map_err(|error| TerminalError::new(format!("ensure signing key: {error}")).into())
}

/// Rotate a tenant OCSF signing key.
pub(crate) async fn rotate_signing_key(
    pool: sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<Uuid, HandlerError> {
    attach_tenant_to_workspace(&pool, tenant_id).await?;
    moa_ocsf::rotate_key(&pool, tenant_id)
        .await
        .map_err(|error| TerminalError::new(format!("rotate signing key: {error}")).into())
}

async fn attach_tenant_to_workspace(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    workspaces::enqueue_tenant_workspace(
        &mut transaction,
        tenant_id,
        moa_authz_schema::TupleOp::Write,
    )
    .await
    .map_err(|error| TerminalError::new(format!("tenant workspace outbox: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}
