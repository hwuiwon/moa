//! Restate service for local API key lifecycle operations.

use moa_auth_providers::api_keys::{CreateApiKeyRequest, CreateApiKeyResponse, KeyListItem};
use moa_authz::FgaClient;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::handlers::authz_shim::{
    journal_context_authz, journal_context_authz_any, require_configured_fga_client,
    require_identity,
};
use crate::identity_admin::api_keys as key_admin;

/// Restate service surface for local API key management.
#[restate_sdk::service]
#[name = "ApiKeys"]
pub trait ApiKeys {
    /// Create a new API key for the caller or an agent.
    async fn create(
        request: Json<CreateApiKeyRequest>,
    ) -> Result<Json<CreateApiKeyResponse>, HandlerError>;

    /// List active API keys owned by the caller.
    async fn list() -> Result<Json<Vec<KeyListItem>>, HandlerError>;

    /// Rotate one API key.
    async fn rotate(id: Json<Uuid>) -> Result<Json<CreateApiKeyResponse>, HandlerError>;

    /// Revoke one API key.
    async fn revoke(id: Json<Uuid>) -> Result<(), HandlerError>;
}

/// Concrete API key management service implementation.
#[derive(Clone)]
pub struct ApiKeysImpl {
    pool: sqlx::PgPool,
    fga_client: Option<FgaClient>,
}

impl ApiKeysImpl {
    /// Creates the API-key service with its persistence and authorization dependencies.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, fga_client: Option<FgaClient>) -> Self {
        Self { pool, fga_client }
    }
}

impl ApiKeys for ApiKeysImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create(
        &self,
        ctx: Context<'_>,
        request: Json<CreateApiKeyRequest>,
    ) -> Result<Json<CreateApiKeyResponse>, HandlerError> {
        annotate_restate_handler_span("ApiKeys", "create");
        let request = request.into_inner();
        validate_key_name(&request.name)?;
        let identity = require_identity(&ctx)?;
        require_tenant_member(self.fga_client.clone(), &ctx, identity.clone()).await?;
        if let Some(agent_id) = request.for_agent_id {
            require_agent_operator_or_tenant_admin(
                self.fga_client.clone(),
                &ctx,
                identity.clone(),
                agent_id,
                identity.tenant_id.0,
            )
            .await?;
        }

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                create_key_for_identity(pool, identity, request)
                    .await
                    .map(Json::from)
            })
            .name("api_keys_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: Lists keys scoped to the trusted caller identity; no caller-supplied resource id is read.
    async fn list(&self, ctx: Context<'_>) -> Result<Json<Vec<KeyListItem>>, HandlerError> {
        annotate_restate_handler_span("ApiKeys", "list");
        let identity = require_identity(&ctx)?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_keys_for_identity(pool, identity).await.map(Json::from) })
            .name("api_keys_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn rotate(
        &self,
        ctx: Context<'_>,
        id: Json<Uuid>,
    ) -> Result<Json<CreateApiKeyResponse>, HandlerError> {
        annotate_restate_handler_span("ApiKeys", "rotate");
        let identity = require_identity(&ctx)?;
        let key_id = id.into_inner();
        let load_pool = self.pool.clone();
        let load_identity = identity.clone();
        let prepared = ctx
            .run(|| async move {
                key_admin::prepare_key_mutation(&load_pool, &load_identity, key_id)
                    .await
                    .map(Json::from)
            })
            .name(format!("api_keys_rotate_load:{key_id}"))
            .await?
            .into_inner();
        authorize_prepared_key(
            self.fga_client.clone(),
            &ctx,
            identity.clone(),
            prepared.authorization,
        )
        .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                key_admin::rotate_prepared_key(pool, identity, prepared)
                    .await
                    .map(Json::from)
            })
            .name("api_keys_rotate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn revoke(&self, ctx: Context<'_>, id: Json<Uuid>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ApiKeys", "revoke");
        let identity = require_identity(&ctx)?;
        let key_id = id.into_inner();
        let load_pool = self.pool.clone();
        let load_identity = identity.clone();
        let prepared = ctx
            .run(|| async move {
                key_admin::prepare_key_mutation(&load_pool, &load_identity, key_id)
                    .await
                    .map(Json::from)
            })
            .name(format!("api_keys_revoke_load:{key_id}"))
            .await?
            .into_inner();
        authorize_prepared_key(
            self.fga_client.clone(),
            &ctx,
            identity.clone(),
            prepared.authorization,
        )
        .await?;
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                key_admin::revoke_prepared_key(pool, identity, prepared, "user_requested").await
            })
            .name("api_keys_revoke")
            .await?)
    }
}

/// Create an API key for an already authenticated identity.
pub async fn create_key_for_identity(
    pool: sqlx::PgPool,
    identity: Identity,
    request: CreateApiKeyRequest,
) -> Result<CreateApiKeyResponse, HandlerError> {
    key_admin::create_key(pool, identity, request).await
}

/// List API keys visible to an already authenticated identity.
pub async fn list_keys_for_identity(
    pool: sqlx::PgPool,
    identity: Identity,
) -> Result<Vec<KeyListItem>, HandlerError> {
    key_admin::list_keys(pool, identity).await
}

/// Rotate an API key for an already authenticated identity.
pub async fn rotate_key_for_identity(
    pool: sqlx::PgPool,
    fga: Option<moa_authz::FgaClient>,
    identity: Identity,
    key_id: Uuid,
) -> Result<CreateApiKeyResponse, HandlerError> {
    key_admin::rotate_key(pool, fga, identity, key_id).await
}

/// Revoke an API key for an already authenticated identity.
pub async fn revoke_key_for_identity(
    pool: sqlx::PgPool,
    fga: Option<moa_authz::FgaClient>,
    identity: Identity,
    key_id: Uuid,
    reason: &str,
) -> Result<(), HandlerError> {
    key_admin::revoke_key(pool, fga, identity, key_id, reason).await
}

fn validate_key_name(name: &str) -> Result<(), HandlerError> {
    if name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "API key name is required").into());
    }
    Ok(())
}

async fn require_tenant_member(
    fga_client: Option<FgaClient>,
    ctx: &Context<'_>,
    identity: Identity,
) -> Result<(), HandlerError> {
    let fga = require_configured_fga_client(fga_client)?;
    let tenant_id = identity.tenant_id;
    journal_context_authz(
        ctx,
        fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
}

async fn require_agent_operator_or_tenant_admin(
    fga_client: Option<FgaClient>,
    ctx: &Context<'_>,
    identity: Identity,
    agent_id: Uuid,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_configured_fga_client(fga_client)?;
    journal_context_authz_any(
        ctx,
        fga,
        identity,
        ObjectType::Agent,
        agent_id,
        Relation::Operator,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
}

async fn authorize_prepared_key(
    fga_client: Option<FgaClient>,
    ctx: &Context<'_>,
    identity: Identity,
    authorization: key_admin::ApiKeyMutationAuthorization,
) -> Result<(), HandlerError> {
    match authorization {
        key_admin::ApiKeyMutationAuthorization::Direct => Ok(()),
        key_admin::ApiKeyMutationAuthorization::TenantAdmin { tenant_id } => {
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
        key_admin::ApiKeyMutationAuthorization::AgentOperatorOrTenantAdmin {
            agent_id,
            tenant_id,
        } => {
            let fga = require_configured_fga_client(fga_client)?;
            journal_context_authz_any(
                ctx,
                fga,
                identity,
                ObjectType::Agent,
                agent_id,
                Relation::Operator,
                ObjectType::Tenant,
                tenant_id,
                Relation::Admin,
            )
            .await
        }
    }
}
