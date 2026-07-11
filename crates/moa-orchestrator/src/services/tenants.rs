//! Restate service for tenant security-audit administration.

use moa_authz::{FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::handlers::authz_shim::{
    require_configured_fga_client, require_identity, translate_authz_error,
};
use crate::identity_admin::tenants as tenant_admin;

/// Request for configuring a tenant audit destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAuditDestinationRequest {
    /// Tenant id.
    pub tenant_id: Uuid,
    /// Destination S3 bucket.
    pub bucket_name: String,
    /// AWS region for the bucket.
    pub region: String,
    /// Optional role to assume before writing.
    pub assume_role_arn: Option<String>,
    /// Object key prefix.
    pub key_prefix: Option<String>,
    /// Object Lock retention in days.
    pub object_lock_days: Option<i32>,
    /// Optional KMS key ARN for server-side encryption.
    pub encryption_kms_key_arn: Option<String>,
}

/// Tenant administration service.
#[restate_sdk::service]
#[name = "Tenants"]
pub trait Tenants {
    /// Ensure a tenant has an active signing key.
    async fn ensure_signing_key(tenant_id: Json<Uuid>) -> Result<Json<Uuid>, HandlerError>;

    /// Rotate a tenant signing key.
    async fn rotate_signing_key(tenant_id: Json<Uuid>) -> Result<Json<Uuid>, HandlerError>;

    /// Configure the S3 audit destination for a tenant.
    async fn set_audit_destination(
        request: Json<SetAuditDestinationRequest>,
    ) -> Result<(), HandlerError>;
}

/// Concrete tenant administration implementation.
#[derive(Clone)]
pub struct TenantsImpl {
    pool: sqlx::PgPool,
    fga_client: Option<FgaClient>,
}

impl TenantsImpl {
    /// Creates the tenant service with its persistence and authorization dependencies.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, fga_client: Option<FgaClient>) -> Self {
        Self { pool, fga_client }
    }
}

impl Tenants for TenantsImpl {
    #[tracing::instrument(skip(self, ctx, tenant_id))]
    async fn ensure_signing_key(
        &self,
        ctx: Context<'_>,
        tenant_id: Json<Uuid>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("Tenants", "ensure_signing_key");
        let identity = require_identity(&ctx)?;
        let tenant_id = tenant_id.into_inner();
        require_tenant_admin(self.fga_client.clone(), &identity, tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                tenant_admin::ensure_signing_key(pool, tenant_id)
                    .await
                    .map(Json)
            })
            .name("tenants_ensure_signing_key")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, tenant_id))]
    async fn rotate_signing_key(
        &self,
        ctx: Context<'_>,
        tenant_id: Json<Uuid>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("Tenants", "rotate_signing_key");
        let identity = require_identity(&ctx)?;
        let tenant_id = tenant_id.into_inner();
        require_tenant_admin(self.fga_client.clone(), &identity, tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move {
                tenant_admin::rotate_signing_key(pool, tenant_id)
                    .await
                    .map(Json)
            })
            .name("tenants_rotate_signing_key")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn set_audit_destination(
        &self,
        ctx: Context<'_>,
        request: Json<SetAuditDestinationRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Tenants", "set_audit_destination");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        validate_destination(&request)?;
        require_tenant_admin(self.fga_client.clone(), &identity, request.tenant_id).await?;
        let pool = self.pool.clone();

        Ok(ctx
            .run(|| async move { tenant_admin::set_audit_destination(pool, request).await })
            .name("tenants_set_audit_destination")
            .await?)
    }
}

async fn require_tenant_admin(
    fga_client: Option<FgaClient>,
    identity: &moa_core::traits::Identity,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_configured_fga_client(fga_client)?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn validate_destination(request: &SetAuditDestinationRequest) -> Result<(), HandlerError> {
    if request.bucket_name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "bucket is required").into());
    }
    if request.region.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "region is required").into());
    }
    if request.object_lock_days.unwrap_or(2190) <= 0 {
        return Err(TerminalError::new_with_code(400, "retention days must be positive").into());
    }
    Ok(())
}
