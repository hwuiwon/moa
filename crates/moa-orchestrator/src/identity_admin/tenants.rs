//! Tenant administration persistence operations.

use restate_sdk::prelude::{HandlerError, TerminalError};
use uuid::Uuid;

use super::workspaces;
use crate::services::tenants::SetAuditDestinationRequest;

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

/// Configure the S3 audit destination for a tenant.
pub(crate) async fn set_audit_destination(
    pool: sqlx::PgPool,
    request: SetAuditDestinationRequest,
) -> Result<(), HandlerError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    workspaces::enqueue_tenant_workspace(
        &mut transaction,
        request.tenant_id,
        moa_authz_schema::TupleOp::Write,
    )
    .await
    .map_err(|error| TerminalError::new(format!("tenant workspace outbox: {error}")))?;
    sqlx::query(
        r#"
        INSERT INTO tenant_audit_destinations
            (tenant_id, bucket_name, region, assume_role_arn,
             key_prefix, object_lock_days, encryption_kms_key_arn)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (tenant_id)
        DO UPDATE SET
            bucket_name = EXCLUDED.bucket_name,
            region = EXCLUDED.region,
            assume_role_arn = EXCLUDED.assume_role_arn,
            key_prefix = EXCLUDED.key_prefix,
            object_lock_days = EXCLUDED.object_lock_days,
            encryption_kms_key_arn = EXCLUDED.encryption_kms_key_arn
        "#,
    )
    .bind(request.tenant_id)
    .bind(&request.bucket_name)
    .bind(&request.region)
    .bind(request.assume_role_arn.as_deref())
    .bind(request.key_prefix.as_deref().unwrap_or("ocsf/"))
    .bind(request.object_lock_days.unwrap_or(2190))
    .bind(request.encryption_kms_key_arn.as_deref())
    .execute(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("set audit destination: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
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
