//! Restate service for agent principal lifecycle operations.

use chrono::{DateTime, Utc};
use moa_auth_providers::api_keys;
use moa_authz::{AuthzCheckError, FgaTuple, enqueue_raw, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use moa_ocsf::ActorInput;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Request body for registering an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterAgentRequest {
    /// Human-readable agent display name.
    pub display_name: String,
}

/// Request body for a `can_act_as` grant mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentActAsRequest {
    /// Agent receiving or losing the delegation relation.
    pub agent_id: Uuid,
    /// User principal the agent may act as.
    pub user_id: Uuid,
}

/// Agent summary returned by list, get, and register.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AgentSummary {
    /// Agent UUID.
    pub id: Uuid,
    /// Tenant UUID.
    pub tenant_id: Uuid,
    /// User who operates the agent. Deactivation cascades can orphan agents.
    pub operator_user_id: Option<Uuid>,
    /// Human-readable agent display name.
    pub display_name: String,
    /// Lifecycle status.
    pub status: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Deactivation timestamp.
    pub deactivated_at: Option<DateTime<Utc>>,
    /// Optional deactivation reason.
    pub deactivated_reason: Option<String>,
}

/// Restate service surface for agent management.
#[restate_sdk::service]
#[name = "Agents"]
pub trait Agents {
    /// Register an agent in the caller's tenant.
    async fn register(
        request: Json<RegisterAgentRequest>,
    ) -> Result<Json<AgentSummary>, HandlerError>;

    /// List active agents operated by the caller.
    async fn list() -> Result<Json<Vec<AgentSummary>>, HandlerError>;

    /// Load one agent by id.
    async fn get(id: Json<Uuid>) -> Result<Json<AgentSummary>, HandlerError>;

    /// Deactivate an agent and revoke its local API keys.
    async fn deactivate(id: Json<Uuid>) -> Result<(), HandlerError>;

    /// Grant an agent the right to act as a user.
    async fn grant_can_act_as(request: Json<AgentActAsRequest>) -> Result<(), HandlerError>;

    /// Revoke an agent's right to act as a user.
    async fn revoke_can_act_as(request: Json<AgentActAsRequest>) -> Result<(), HandlerError>;
}

/// Concrete agent service implementation.
#[derive(Clone, Default)]
pub struct AgentsImpl;

impl Agents for AgentsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn register(
        &self,
        ctx: Context<'_>,
        request: Json<RegisterAgentRequest>,
    ) -> Result<Json<AgentSummary>, HandlerError> {
        annotate_restate_handler_span("Agents", "register");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        validate_agent_name(&request.display_name)?;
        require_tenant_admin(&identity).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                register_agent_inner(pool, identity, request)
                    .await
                    .map(Json::from)
            })
            .name("agents_register")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn list(&self, ctx: Context<'_>) -> Result<Json<Vec<AgentSummary>>, HandlerError> {
        annotate_restate_handler_span("Agents", "list");
        let identity = require_identity(&ctx)?;
        require_tenant_member(&identity).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move { list_agents_inner(pool, identity).await.map(Json::from) })
            .name("agents_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn get(
        &self,
        ctx: Context<'_>,
        id: Json<Uuid>,
    ) -> Result<Json<AgentSummary>, HandlerError> {
        annotate_restate_handler_span("Agents", "get");
        let identity = require_identity(&ctx)?;
        let agent_id = id.into_inner();
        require_agent_operator_or_tenant_admin(&identity, agent_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                load_agent(&pool, agent_id)
                    .await
                    .and_then(|agent| ensure_same_tenant(agent, identity.tenant_id))
                    .map(Json::from)
            })
            .name("agents_get")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, id))]
    async fn deactivate(&self, ctx: Context<'_>, id: Json<Uuid>) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Agents", "deactivate");
        let identity = require_identity(&ctx)?;
        let agent_id = id.into_inner();
        require_agent_operator_or_tenant_admin(&identity, agent_id).await?;
        let fga = require_fga_client()?;
        let agent_wire = format!("agent:{agent_id}");
        let can_act_as = fga
            .read(None, Some("can_act_as"), Some(&agent_wire))
            .await
            .map_err(|error| {
                tracing::error!(error = %error, "read agent can_act_as tuples failed");
                TerminalError::new_with_code(503, "authorization engine unavailable")
            })?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(
            ctx.run(|| async move {
                deactivate_agent_inner(pool, identity, agent_id, can_act_as).await
            })
            .name("agents_deactivate")
            .await?,
        )
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn grant_can_act_as(
        &self,
        ctx: Context<'_>,
        request: Json<AgentActAsRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Agents", "grant_can_act_as");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_grant_authority(&identity, request.agent_id, request.user_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                mutate_can_act_as_inner(pool, identity, request, TupleOp::Write).await
            })
            .name("agents_grant_can_act_as")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn revoke_can_act_as(
        &self,
        ctx: Context<'_>,
        request: Json<AgentActAsRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Agents", "revoke_can_act_as");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        require_agent_operator_or_tenant_admin(&identity, request.agent_id).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                mutate_can_act_as_inner(pool, identity, request, TupleOp::Delete).await
            })
            .name("agents_revoke_can_act_as")
            .await?)
    }
}

async fn register_agent_inner(
    pool: PgPool,
    identity: Identity,
    request: RegisterAgentRequest,
) -> Result<AgentSummary, HandlerError> {
    let operator_user_id = required_operator_user_id(&identity)?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let agent: AgentSummary = sqlx::query_as(
        r#"
        INSERT INTO agents
            (tenant_id, operator_user_id, display_name)
        VALUES ($1, $2, $3)
        RETURNING id, tenant_id, operator_user_id, display_name,
                  status, created_at, deactivated_at, deactivated_reason
        "#,
    )
    .bind(identity.tenant_id)
    .bind(operator_user_id)
    .bind(request.display_name.trim())
    .fetch_one(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("register agent: {error}")))?;

    enqueue_agent_tuples(&mut transaction, TupleOp::Write, &agent).await?;
    moa_ocsf::emit_agent_registered_tx(&mut transaction, identity.tenant_id, &identity, agent.id)
        .await
        .map_err(|error| TerminalError::new(format!("audit agent register: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(agent)
}

async fn list_agents_inner(
    pool: PgPool,
    identity: Identity,
) -> Result<Vec<AgentSummary>, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, tenant_id, operator_user_id, display_name,
               status, created_at, deactivated_at, deactivated_reason
        FROM agents
        WHERE tenant_id = $1
          AND operator_user_id = $2
          AND status != 'deactivated'
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(identity.tenant_id)
    .bind(identity.id)
    .fetch_all(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("list agents: {error}")).into())
}

async fn deactivate_agent_inner(
    pool: PgPool,
    identity: Identity,
    agent_id: Uuid,
    can_act_as: Vec<FgaTuple>,
) -> Result<(), HandlerError> {
    let agent = ensure_same_tenant(load_agent(&pool, agent_id).await?, identity.tenant_id)?;
    let actor_user_id = actor_user_id(&identity);
    let actor = ActorInput::from_identity(&identity);
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    sqlx::query(
        r#"
        UPDATE agents
        SET status = 'deactivated',
            deactivated_at = COALESCE(deactivated_at, NOW()),
            deactivated_reason = COALESCE(deactivated_reason, 'user_requested')
        WHERE id = $1
        "#,
    )
    .bind(agent.id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("deactivate agent: {error}")))?;

    for tuple in can_act_as {
        enqueue_raw(
            &mut *transaction,
            TupleOp::Delete,
            &tuple.user,
            &tuple.relation,
            &tuple.object,
            Some(agent.tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("agent delegation outbox: {error}")))?;
    }
    enqueue_agent_tuples(&mut transaction, TupleOp::Delete, &agent).await?;
    revoke_agent_api_keys(&mut transaction, agent.id, actor_user_id, actor.clone()).await?;
    moa_ocsf::emit_agent_deactivated_tx(&mut transaction, agent.tenant_id, &identity, agent.id)
        .await
        .map_err(|error| TerminalError::new(format!("audit agent deactivate: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn mutate_can_act_as_inner(
    pool: PgPool,
    identity: Identity,
    request: AgentActAsRequest,
    op: TupleOp,
) -> Result<(), HandlerError> {
    let agent = ensure_same_tenant(
        load_agent(&pool, request.agent_id).await?,
        identity.tenant_id,
    )?;
    if agent.status != "active" {
        return Err(TerminalError::new_with_code(409, "agent is not active").into());
    }
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    enqueue_raw(
        &mut *transaction,
        op,
        &format!("user:{}", request.user_id),
        "can_act_as",
        &format!("agent:{}", request.agent_id),
        Some(identity.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("agent can_act_as outbox: {error}")))?;
    match op {
        TupleOp::Write => {
            moa_ocsf::emit_delegation_granted_tx(
                &mut transaction,
                identity.tenant_id,
                &identity,
                request.agent_id,
                request.user_id,
            )
            .await
        }
        TupleOp::Delete => {
            moa_ocsf::emit_delegation_revoked_tx(
                &mut transaction,
                identity.tenant_id,
                &identity,
                request.agent_id,
                request.user_id,
            )
            .await
        }
    }
    .map_err(|error| TerminalError::new(format!("audit agent delegation: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

async fn revoke_agent_api_keys(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    agent_id: Uuid,
    actor_user_id: Option<Uuid>,
    actor: ActorInput,
) -> Result<(), HandlerError> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        r#"
        SELECT id, tenant_id
        FROM api_keys
        WHERE owner_agent_id = $1 AND revoked_at IS NULL
        "#,
    )
    .bind(agent_id)
    .fetch_all(&mut **transaction)
    .await
    .map_err(|error| TerminalError::new(format!("load agent api keys: {error}")))?;

    for (key_id, tenant_id) in rows {
        api_keys::revoke(
            &mut **transaction,
            key_id,
            "agent_deactivation_cascade",
            actor_user_id,
        )
        .await
        .map_err(|error| TerminalError::new(format!("revoke agent api key: {error}")))?;
        enqueue_raw(
            &mut **transaction,
            TupleOp::Delete,
            &format!("agent:{agent_id}"),
            "owner",
            &format!("api_key:{key_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("api key owner outbox: {error}")))?;
        enqueue_raw(
            &mut **transaction,
            TupleOp::Delete,
            &format!("tenant:{tenant_id}"),
            "tenant",
            &format!("api_key:{key_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("api key tenant outbox: {error}")))?;
        enqueue_raw(
            &mut **transaction,
            TupleOp::Delete,
            &format!("api_key:{key_id}"),
            "member",
            &format!("tenant:{tenant_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("api key member outbox: {error}")))?;
        moa_ocsf::emit_api_key_revoked_tx(
            transaction,
            tenant_id,
            actor.clone(),
            key_id,
            Some("agent_deactivation_cascade"),
        )
        .await
        .map_err(|error| TerminalError::new(format!("audit agent api key revoke: {error}")))?;
    }
    Ok(())
}

async fn enqueue_agent_tuples(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    agent: &AgentSummary,
) -> Result<(), HandlerError> {
    enqueue_raw(
        &mut **transaction,
        op,
        &format!("tenant:{}", agent.tenant_id),
        "tenant",
        &format!("agent:{}", agent.id),
        Some(agent.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("agent tenant outbox: {error}")))?;
    if let Some(operator_user_id) = agent.operator_user_id {
        enqueue_raw(
            &mut **transaction,
            op,
            &format!("user:{operator_user_id}"),
            "operator",
            &format!("agent:{}", agent.id),
            Some(agent.tenant_id),
        )
        .await
        .map_err(|error| TerminalError::new(format!("agent operator outbox: {error}")))?;
    }
    Ok(())
}

async fn load_agent(pool: &PgPool, agent_id: Uuid) -> Result<AgentSummary, HandlerError> {
    sqlx::query_as(
        r#"
        SELECT id, tenant_id, operator_user_id, display_name,
               status, created_at, deactivated_at, deactivated_reason
        FROM agents
        WHERE id = $1
        "#,
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| TerminalError::new(format!("load agent: {error}")))?
    .ok_or_else(|| TerminalError::new_with_code(404, "agent not found").into())
}

fn ensure_same_tenant(agent: AgentSummary, tenant_id: Uuid) -> Result<AgentSummary, HandlerError> {
    if agent.tenant_id == tenant_id {
        return Ok(agent);
    }
    Err(TerminalError::new_with_code(404, "agent not found").into())
}

fn validate_agent_name(name: &str) -> Result<(), HandlerError> {
    if name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "agent name is required").into());
    }
    Ok(())
}

fn required_operator_user_id(identity: &Identity) -> Result<Uuid, HandlerError> {
    if let Some(user_id) = identity.acting_on_behalf_of {
        return Ok(user_id);
    }
    if identity.identity_type == IdentityType::User {
        return Ok(identity.id);
    }
    Err(TerminalError::new_with_code(403, "agent registration requires a user operator").into())
}

async fn require_grant_authority(
    identity: &Identity,
    agent_id: Uuid,
    user_id: Uuid,
) -> Result<(), HandlerError> {
    if actor_user_id(identity) == Some(user_id) {
        return require_agent_operator_or_tenant_admin(identity, agent_id).await;
    }
    require_tenant_admin(identity).await
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

async fn require_tenant_admin(identity: &Identity) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

async fn require_agent_operator_or_tenant_admin(
    identity: &Identity,
    agent_id: Uuid,
) -> Result<(), HandlerError> {
    let fga = require_fga_client()?;
    match require_authz_with_delegation(
        &fga,
        identity,
        ObjectType::Agent,
        agent_id,
        Relation::Operator,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(AuthzCheckError::Forbidden { .. }) => require_tenant_admin(identity).await,
        Err(error) => Err(translate_authz_error(error)),
    }
}

fn actor_user_id(identity: &Identity) -> Option<Uuid> {
    match identity.identity_type {
        IdentityType::User => Some(identity.id),
        IdentityType::Agent | IdentityType::Service => identity.acting_on_behalf_of,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(
        identity_type: IdentityType,
        id: Uuid,
        acting_on_behalf_of: Option<Uuid>,
    ) -> Identity {
        Identity {
            identity_type,
            id,
            tenant_id: Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
                .expect("tenant fixture UUID should parse"),
            api_key_id: None,
            acting_on_behalf_of,
        }
    }

    #[test]
    fn required_operator_user_id_accepts_user_identity() {
        // Pins: a direct human caller becomes the operator for newly registered agents.
        let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("user fixture UUID should parse");
        let identity = identity(IdentityType::User, user_id, None);

        let operator_user_id =
            required_operator_user_id(&identity).expect("user identity should be accepted");

        assert_eq!(operator_user_id, user_id);
    }

    #[test]
    fn required_operator_user_id_accepts_delegated_identity() {
        // Pins: delegated service or agent callers register agents for the real user they represent.
        let service_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("service fixture UUID should parse");
        let user_id = Uuid::parse_str("33333333-3333-3333-3333-333333333333")
            .expect("delegated user fixture UUID should parse");
        let identity = identity(IdentityType::Service, service_id, Some(user_id));

        let operator_user_id =
            required_operator_user_id(&identity).expect("delegated identity should be accepted");

        assert_eq!(operator_user_id, user_id);
    }

    #[test]
    fn required_operator_user_id_rejects_service_without_delegation() {
        // Pins: service callers cannot become durable agent operators without a real user.
        let service_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("service fixture UUID should parse");
        let identity = identity(IdentityType::Service, service_id, None);

        let error = required_operator_user_id(&identity)
            .expect_err("service identity without delegation should be rejected");
        let error_ref =
            <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(&error);

        assert_eq!(
            error_ref.to_string(),
            "Terminal error [403]: agent registration requires a user operator"
        );
    }
}
