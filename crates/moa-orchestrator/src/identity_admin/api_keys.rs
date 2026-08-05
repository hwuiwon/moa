//! Local API-key repository and lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::api_keys::{
    self, CreateApiKeyRequest, CreateApiKeyResponse, Env, KeyListItem, KeyOwner, NewApiKey,
};
use moa_authz::{
    FgaClient, enqueue_raw, fga_subject, require_authz, require_authz_with_delegation,
};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::traits::{Identity, IdentityType};
use moa_ocsf::ActorInput;
use restate_sdk::prelude::{HandlerError, TerminalError};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::handlers::authz_shim::translate_authz_error;

type KeyListRow = (
    Uuid,
    String,
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

/// Create a new API key for the caller or an agent.
pub(crate) async fn create_key(
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
            tenant_id: identity.tenant_id.0,
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
        identity.tenant_id.0,
        owner,
    )
    .await?;
    moa_ocsf::emit_api_key_created_tx(&mut transaction, identity.tenant_id.0, &identity, issued.id)
        .await
        .map_err(|error| TerminalError::new(format!("audit api key create: {error}")))?;
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

/// List active API keys owned by the caller.
pub(crate) async fn list_keys(
    pool: PgPool,
    identity: Identity,
) -> Result<Vec<KeyListItem>, HandlerError> {
    if let Some(api_key_id) = identity.api_key_id {
        let rows: Vec<KeyListRow> = sqlx::query_as(
            r#"
            SELECT id, name, prefix, env, created_at, last_used_at
            FROM api_keys
            WHERE id = $1
              AND tenant_id = $2
              AND revoked_at IS NULL
            ORDER BY created_at DESC
            "#,
        )
        .bind(api_key_id)
        .bind(identity.tenant_id)
        .fetch_all(&pool)
        .await
        .map_err(|error| TerminalError::new(format!("list presenting api key: {error}")))?;

        return Ok(key_list_items(rows));
    }

    let (owner_user_id, owner_agent_id) = match identity.identity_type {
        IdentityType::Operator => (Some(identity.id), None),
        IdentityType::Agent => (None, Some(identity.id)),
        IdentityType::Service | IdentityType::Contact => {
            return Err(
                TerminalError::new_with_code(403, "identity type cannot list API keys").into(),
            );
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

    Ok(key_list_items(rows))
}

fn key_list_items(rows: Vec<KeyListRow>) -> Vec<KeyListItem> {
    rows.into_iter()
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
        .collect()
}

/// Rotate one API key after loading it through a management-safe scope.
pub(crate) async fn rotate_key(
    pool: PgPool,
    fga: Option<FgaClient>,
    identity: Identity,
    key_id: Uuid,
) -> Result<CreateApiKeyResponse, HandlerError> {
    let old = load_manageable_active_key(&pool, fga.as_ref(), &identity, key_id).await?;
    rotate_key_row(pool, identity, old).await
}

/// Revoke one API key after loading it through a management-safe scope.
pub(crate) async fn revoke_key(
    pool: PgPool,
    fga: Option<FgaClient>,
    identity: Identity,
    key_id: Uuid,
    reason: &str,
) -> Result<(), HandlerError> {
    let row = load_manageable_active_key(&pool, fga.as_ref(), &identity, key_id).await?;
    revoke_key_row(pool, identity, row, reason).await
}

/// Authorization work required before mutating one journaled API-key row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ApiKeyMutationAuthorization {
    /// The authenticated principal directly owns or presents this key.
    Direct,
    /// The caller must be an admin of the key's tenant.
    TenantAdmin { tenant_id: Uuid },
    /// The caller may operate the owning agent or administer its tenant.
    AgentOperatorOrTenantAdmin { agent_id: Uuid, tenant_id: Uuid },
}

/// Replay-stable API-key row and the separate authorization decision it requires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PreparedApiKeyMutation {
    row: ApiKeyRow,
    /// Authorization branch derived exclusively from the journaled database row.
    pub authorization: ApiKeyMutationAuthorization,
}

/// Loads one active API key and derives its authorization branch without calling OpenFGA.
pub(crate) async fn prepare_key_mutation(
    pool: &PgPool,
    identity: &Identity,
    key_id: Uuid,
) -> Result<PreparedApiKeyMutation, HandlerError> {
    let row = if identity.api_key_id.is_some() {
        load_active_key_by_id(pool, key_id).await?
    } else {
        load_active_key_for_tenant(pool, key_id, identity.tenant_id.0).await?
    };
    let directly_authorized = row.tenant_id == identity.tenant_id.0
        && (identity.api_key_id == Some(key_id)
            || (identity.api_key_id.is_none()
                && (row.owner_user_id == Some(identity.id)
                    || row.owner_agent_id == Some(identity.id))));
    let authorization = if directly_authorized {
        ApiKeyMutationAuthorization::Direct
    } else if let Some(agent_id) = row.owner_agent_id {
        ApiKeyMutationAuthorization::AgentOperatorOrTenantAdmin {
            agent_id,
            tenant_id: row.tenant_id,
        }
    } else {
        ApiKeyMutationAuthorization::TenantAdmin {
            tenant_id: row.tenant_id,
        }
    };
    Ok(PreparedApiKeyMutation { row, authorization })
}

/// Rotates one already loaded and separately authorized API key.
pub(crate) async fn rotate_prepared_key(
    pool: PgPool,
    identity: Identity,
    prepared: PreparedApiKeyMutation,
) -> Result<CreateApiKeyResponse, HandlerError> {
    rotate_key_row(pool, identity, prepared.row).await
}

/// Revokes one already loaded and separately authorized API key.
pub(crate) async fn revoke_prepared_key(
    pool: PgPool,
    identity: Identity,
    prepared: PreparedApiKeyMutation,
    reason: &str,
) -> Result<(), HandlerError> {
    revoke_key_row(pool, identity, prepared.row, reason).await
}

async fn rotate_key_row(
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
    moa_ocsf::emit_api_key_revoked_tx(
        &mut transaction,
        old.tenant_id,
        ActorInput::from_identity(&identity),
        old.id,
        Some("rotation"),
    )
    .await
    .map_err(|error| TerminalError::new(format!("audit api key rotate revoke: {error}")))?;

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
    moa_ocsf::emit_api_key_created_tx(&mut transaction, old.tenant_id, &identity, issued.id)
        .await
        .map_err(|error| TerminalError::new(format!("audit api key rotate create: {error}")))?;

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

async fn revoke_key_row(
    pool: PgPool,
    identity: Identity,
    row: ApiKeyRow,
    reason: &str,
) -> Result<(), HandlerError> {
    let owner = row.owner()?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    api_keys::revoke(&mut *transaction, row.id, reason, actor_user_id(&identity))
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
    moa_ocsf::emit_api_key_revoked_tx(
        &mut transaction,
        row.tenant_id,
        ActorInput::from_identity(&identity),
        row.id,
        Some(reason),
    )
    .await
    .map_err(|error| TerminalError::new(format!("audit api key revoke: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn load_manageable_active_key(
    pool: &PgPool,
    fga: Option<&FgaClient>,
    identity: &Identity,
    key_id: Uuid,
) -> Result<ApiKeyRow, HandlerError> {
    if let Some(presenting_key_id) = identity.api_key_id {
        if key_id == presenting_key_id {
            return load_active_key_for_tenant(pool, key_id, identity.tenant_id.0).await;
        }

        let row = load_active_key_by_id(pool, key_id).await?;
        let Some(fga) = fga else {
            return Err(
                TerminalError::new_with_code(503, "authorization engine unavailable").into(),
            );
        };
        require_authz(
            fga,
            identity,
            ObjectType::Tenant,
            row.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;
        return Ok(row);
    }

    if let Some(row) = load_direct_owner_key(pool, key_id, identity).await? {
        return Ok(row);
    }
    let Some(fga) = fga else {
        return Err(TerminalError::new_with_code(503, "authorization engine unavailable").into());
    };
    if let Some(row) = load_operator_key(pool, fga, key_id, identity).await? {
        return Ok(row);
    }

    require_authz_with_delegation(
        fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)?;
    load_active_key_for_tenant(pool, key_id, identity.tenant_id.0).await
}

async fn load_active_key_by_id(pool: &PgPool, key_id: Uuid) -> Result<ApiKeyRow, HandlerError> {
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
    .map_err(|error| TerminalError::new(format!("load api key by id: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "API key not found").into())
}

async fn load_direct_owner_key(
    pool: &PgPool,
    key_id: Uuid,
    identity: &Identity,
) -> Result<Option<ApiKeyRow>, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, owner_user_id, owner_agent_id, tenant_id, name, description, env
        FROM api_keys
        WHERE id = $1
          AND tenant_id = $2
          AND revoked_at IS NULL
          AND (owner_user_id = $3 OR owner_agent_id = $3)
        "#,
    )
    .bind(key_id)
    .bind(identity.tenant_id)
    .bind(identity.id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load owned api key: {error}")).into())
}

async fn load_operator_key(
    pool: &PgPool,
    fga: &FgaClient,
    key_id: Uuid,
    identity: &Identity,
) -> Result<Option<ApiKeyRow>, HandlerError> {
    let subject = fga_subject(identity);
    let agent_ids = fga
        .list_objects("agent", "operator", &subject)
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "list operator agents for API-key management failed");
            TerminalError::new_with_code(503, "authorization engine unavailable")
        })?
        .into_iter()
        .filter_map(|object| {
            object
                .strip_prefix("agent:")
                .and_then(|value| Uuid::parse_str(value).ok())
        })
        .collect::<Vec<_>>();
    if agent_ids.is_empty() {
        return Ok(None);
    }

    let row: Option<ApiKeyRow> = sqlx::query_as(
        r#"
        SELECT id, owner_user_id, owner_agent_id, tenant_id, name, description, env
        FROM api_keys
        WHERE id = $1
          AND tenant_id = $2
          AND revoked_at IS NULL
          AND owner_agent_id = ANY($3)
        "#,
    )
    .bind(key_id)
    .bind(identity.tenant_id)
    .bind(&agent_ids)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load operator api key: {error}")))?;

    let Some(row) = row else {
        return Ok(None);
    };
    if let Some(agent_id) = row.owner_agent_id {
        require_authz_with_delegation(
            fga,
            identity,
            ObjectType::Agent,
            agent_id,
            Relation::Operator,
        )
        .await
        .map_err(translate_authz_error)?;
    }
    Ok(Some(row))
}

async fn load_active_key_for_tenant(
    pool: &PgPool,
    key_id: Uuid,
    tenant_id: Uuid,
) -> Result<ApiKeyRow, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, owner_user_id, owner_agent_id, tenant_id, name, description, env
        FROM api_keys
        WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL
        "#,
    )
    .bind(key_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load api key: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "API key not found").into())
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
    if op == TupleOp::Delete {
        enqueue_key_tenant_role_deletes(transaction, key_id, tenant_id).await?;
    }
    Ok(())
}

async fn enqueue_key_tenant_role_deletes(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key_id: Uuid,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let key_wire = format!("api_key:{key_id}");
    let tenant_wire = format!("tenant:{tenant_id}");
    for relation in ["admin", "operator"] {
        enqueue_raw(
            &mut **transaction,
            TupleOp::Delete,
            &key_wire,
            relation,
            &tenant_wire,
            Some(tenant_id),
        )
        .await
        .map_err(|error| {
            TerminalError::new(format!("api key tenant role {relation} outbox: {error}"))
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
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

fn actor_user_id(identity: &Identity) -> Option<Uuid> {
    match identity.identity_type {
        IdentityType::Operator => Some(identity.id),
        IdentityType::Agent | IdentityType::Service | IdentityType::Contact => {
            identity.acting_on_behalf_of
        }
    }
}

fn owner_wire(owner: KeyOwner) -> String {
    match owner {
        KeyOwner::User(user_id) => format!("operator:{user_id}"),
        KeyOwner::Agent(agent_id) => format!("agent:{agent_id}"),
    }
}
