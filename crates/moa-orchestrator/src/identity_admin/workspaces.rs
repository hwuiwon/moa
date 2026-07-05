//! Workspace authorization tuple helpers.

use moa_authz::{AuthzError, enqueue_raw};
use moa_authz_schema::TupleOp;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

/// Enqueue the tuple that attaches a tenant to the deployment workspace.
pub(crate) async fn enqueue_tenant_workspace(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    op: TupleOp,
) -> Result<(), AuthzError> {
    enqueue_raw(
        &mut **tx,
        op,
        &format!("workspace:{}", moa_core::WORKSPACE_ID),
        "workspace",
        &format!("tenant:{tenant_id}"),
        Some(tenant_id),
    )
    .await
}
