//! Restate service for tenant security-audit administration.

use moa_authz::FgaClient;
use moa_authz_schema::{ObjectType, Relation};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::handlers::authz_shim::{
    journal_context_authz, require_configured_fga_client, require_identity,
};
use crate::identity_admin::tenants as tenant_admin;

/// Tenant administration service.
#[restate_sdk::service]
#[name = "Tenants"]
pub trait Tenants {
    /// Ensure a tenant has an active signing key.
    async fn ensure_signing_key(tenant_id: Json<Uuid>) -> Result<Json<Uuid>, HandlerError>;

    /// Rotate a tenant signing key.
    async fn rotate_signing_key(tenant_id: Json<Uuid>) -> Result<Json<Uuid>, HandlerError>;
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
        require_tenant_admin(self.fga_client.clone(), &ctx, identity, tenant_id).await?;
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
        require_tenant_admin(self.fga_client.clone(), &ctx, identity, tenant_id).await?;
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
}

async fn require_tenant_admin(
    fga_client: Option<FgaClient>,
    ctx: &Context<'_>,
    identity: moa_core::traits::Identity,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_configured_fga_client(fga_client)?;
    journal_context_authz(
        ctx,
        fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
}
