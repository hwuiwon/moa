//! Restate service for local API key lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::api_keys::{
    self, CreateApiKeyRequest, CreateApiKeyResponse, Env, KeyListItem, KeyOwner, NewApiKey,
};
use moa_authz::{AuthzCheckError, enqueue_raw, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use restate_sdk::prelude::*;
use secrecy::ExposeSecret;
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

type KeyListRow = (
    Uuid,
    String,
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

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
#[derive(Clone, Default)]
pub struct ApiKeysImpl;

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
        require_tenant_member(&identity).await?;
        if let Some(agent_id) = request.for_agent_id {
            require_agent_operator_or_tenant_admin(&identity, agent_id).await?;
        }

        let pool = OrchestratorCtx::current().graph_pool.clone();
        Ok(ctx
            .run(|| async move {
                create_key_inner(pool, identity, request)
                    .await
                    .map(Json::from)
            })
            .name("api_keys_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn list(&self, ctx: Context<'_>) -> Result<Json<Vec<KeyListItem>>, HandlerError> {
        annotate_restate_handler_span("ApiKeys", "list");
        let identity = require_identity(&ctx)?;
        let pool = OrchestratorCtx::current().graph_pool.clone();
        Ok(ctx
            .run(|| async move { list_keys_inner(pool, identity).await.map(Json::from) })
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
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let row = load_active_key(&pool, key_id).await?;
        authorize_key_management(&identity, &row).await?;

        Ok(ctx
            .run(|| async move { rotate_key_inner(pool, identity, row).await.map(Json::from) })
            .name("api_keys_rotate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn revoke(&self, ctx: Context<'_>, id: Json<Uuid>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ApiKeys", "revoke");
        let identity = require_identity(&ctx)?;
        let key_id = id.into_inner();
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let row = load_active_key(&pool, key_id).await?;
        authorize_key_management(&identity, &row).await?;

        Ok(ctx
            .run(|| async move {
                revoke_key_inner(pool, row, "user_requested", actor_user_id(&identity)).await
            })
            .name("api_keys_revoke")
            .await?)
    }
}

async fn create_key_inner(
    pool: PgPool,
    identity: Identity,
    request: CreateApiKeyRequest,
) -> Result<CreateApiKeyResponse, HandlerError> {
    let owner = request
        .for_agent_id
        .map(KeyOwner::Agent)
        .unwrap_or(KeyOwner::User(identity.id));
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let issued = api_keys::create(
        &mut *transaction,
        NewApiKey {
            tenant_id: identity.tenant_id,
            owner,
            env: request.env,
            name: &request.name,
            description: request.description.as_deref(),
        },
    )
    .await
    .map_err(|error| TerminalError::new(format!("api key create: {error}")))?;

    enqueue_key_scope_tuples(
        &mut transaction,
        TupleOp::Write,
        issued.id,
        identity.tenant_id,
        owner,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(CreateApiKeyResponse {
        id: issued.id,
        key: issued.key.expose_secret().to_string(),
        prefix: issued.prefix,
    })
}

async fn list_keys_inner(
    pool: PgPool,
    identity: Identity,
) -> Result<Vec<KeyListItem>, HandlerError> {
    let (owner_user_id, owner_agent_id) = match identity.identity_type {
        IdentityType::User => (Some(identity.id), None),
        IdentityType::Agent => (None, Some(identity.id)),
        IdentityType::Service => {
            return Err(TerminalError::new_with_code(
                403,
                "service identities cannot list API keys",
            )
            .into());
        }
    };

    let rows: Vec<KeyListRow> = sqlx::query_as(
        r#"
            SELECT id, name, prefix, env, created_at, last_used_at
            FROM api_keys
            WHERE revoked_at IS NULL
              AND (($1::UUID IS NOT NULL AND owner_user_id = $1)
                   OR ($2::UUID IS NOT NULL AND owner_agent_id = $2))
            ORDER BY created_at DESC
            "#,
    )
    .bind(owner_user_id)
    .bind(owner_agent_id)
    .fetch_all(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("list api keys: {error}")))?;

    Ok(rows
        .into_iter()
        .map(
            |(id, name, prefix, env, created_at, last_used_at)| KeyListItem {
                id,
                name,
                prefix,
                env,
                created_at,
                last_used_at,
            },
        )
        .collect())
}

async fn rotate_key_inner(
    pool: PgPool,
    identity: Identity,
    old: ApiKeyRow,
) -> Result<CreateApiKeyResponse, HandlerError> {
    let env = Env::parse(&old.env)
        .ok_or_else(|| TerminalError::new(format!("stored key has invalid env `{}`", old.env)))?;
    let owner = old.owner()?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

    api_keys::revoke(
        &mut *transaction,
        old.id,
        "rotation",
        actor_user_id(&identity),
    )
    .await
    .map_err(|error| TerminalError::new(format!("revoke old api key: {error}")))?;
    enqueue_key_scope_tuples(
        &mut transaction,
        TupleOp::Delete,
        old.id,
        old.tenant_id,
        owner,
    )
    .await?;

    let issued = api_keys::create(
        &mut *transaction,
        NewApiKey {
            tenant_id: old.tenant_id,
            owner,
            env,
            name: &old.name,
            description: old.description.as_deref(),
        },
    )
    .await
    .map_err(|error| TerminalError::new(format!("create rotated api key: {error}")))?;
    enqueue_key_scope_tuples(
        &mut transaction,
        TupleOp::Write,
        issued.id,
        old.tenant_id,
        owner,
    )
    .await?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(CreateApiKeyResponse {
        id: issued.id,
        key: issued.key.expose_secret().to_string(),
        prefix: issued.prefix,
    })
}

async fn revoke_key_inner(
    pool: PgPool,
    row: ApiKeyRow,
    reason: &str,
    actor_user_id: Option<Uuid>,
) -> Result<(), HandlerError> {
    let owner = row.owner()?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    api_keys::revoke(&mut *transaction, row.id, reason, actor_user_id)
        .await
        .map_err(|error| TerminalError::new(format!("revoke api key: {error}")))?;
    enqueue_key_scope_tuples(
        &mut transaction,
        TupleOp::Delete,
        row.id,
        row.tenant_id,
        owner,
    )
    .await?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn enqueue_key_scope_tuples(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    key_id: Uuid,
    tenant_id: Uuid,
    owner: KeyOwner,
) -> Result<(), HandlerError> {
    let owner_wire = owner_wire(owner);
    let key_wire = format!("api_key:{key_id}");
    let tenant_wire = format!("tenant:{tenant_id}");

    enqueue_raw(
        &mut **transaction,
        op,
        &owner_wire,
        "owner",
        &key_wire,
        Some(tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("api key owner outbox: {error}")))?;
    enqueue_raw(
        &mut **transaction,
        op,
        &tenant_wire,
        "tenant",
        &key_wire,
        Some(tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("api key tenant outbox: {error}")))?;
    enqueue_raw(
        &mut **transaction,
        op,
        &key_wire,
        "member",
        &tenant_wire,
        Some(tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("api key tenant-member outbox: {error}")))?;
    Ok(())
}

fn validate_key_name(name: &str) -> Result<(), HandlerError> {
    if name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "API key name is required").into());
    }
    Ok(())
}

async fn require_tenant_member(identity: &Identity) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Member,
    )
    .await
    .map_err(translate_authz_error)
}

async fn require_agent_operator_or_tenant_admin(
    identity: &Identity,
    agent_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    let operator = require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Agent,
        agent_id,
        Relation::Operator,
    )
    .await;
    match operator {
        Ok(()) => Ok(()),
        Err(AuthzCheckError::Forbidden { .. }) => require_authz_with_delegation(
            &fga,
            identity,
            ObjectType::Tenant,
            identity.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error),
        Err(error) => Err(translate_authz_error(error)),
    }
}

async fn authorize_key_management(
    identity: &Identity,
    row: &ApiKeyRow,
) -> Result<(), HandlerError> {
    if row.owner_user_id == Some(identity.id) || row.owner_agent_id == Some(identity.id) {
        return Ok(());
    }
    if let Some(agent_id) = row.owner_agent_id {
        let operator = require_agent_operator_or_tenant_admin(identity, agent_id).await;
        if operator.is_ok() {
            return Ok(());
        }
    }

    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        row.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn actor_user_id(identity: &Identity) -> Option<Uuid> {
    match identity.identity_type {
        IdentityType::User => Some(identity.id),
        IdentityType::Agent | IdentityType::Service => identity.acting_on_behalf_of,
    }
}

fn owner_wire(owner: KeyOwner) -> String {
    match owner {
        KeyOwner::User(user_id) => format!("user:{user_id}"),
        KeyOwner::Agent(agent_id) => format!("agent:{agent_id}"),
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ApiKeyRow {
    id: Uuid,
    owner_user_id: Option<Uuid>,
    owner_agent_id: Option<Uuid>,
    tenant_id: Uuid,
    name: String,
    description: Option<String>,
    env: String,
}

impl ApiKeyRow {
    fn owner(&self) -> Result<KeyOwner, HandlerError> {
        match (self.owner_user_id, self.owner_agent_id) {
            (Some(user_id), None) => Ok(KeyOwner::User(user_id)),
            (None, Some(agent_id)) => Ok(KeyOwner::Agent(agent_id)),
            _ => Err(TerminalError::new("api key owner invariant violated").into()),
        }
    }
}

async fn load_active_key(pool: &PgPool, key_id: Uuid) -> Result<ApiKeyRow, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, owner_user_id, owner_agent_id, tenant_id, name, description, env
        FROM api_keys
        WHERE id = $1 AND revoked_at IS NULL
        "#,
    )
    .bind(key_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load api key: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "API key not found").into())
}
