//! Restate service for agent principal lifecycle operations.

use chrono::{DateTime, Utc};
use moa_authz::{AuthzCheckError, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::identity_admin::agents as agent_admin;

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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                agent_admin::register_agent(pool, identity, request)
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                agent_admin::list_agents(pool, identity)
                    .await
                    .map(Json::from)
            })
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                agent_admin::get_agent(pool, identity, agent_id)
                    .await
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
        let can_act_as = ctx
            .run(|| async move {
                fga.read(None, Some("can_act_as"), Some(agent_wire.as_str()))
                    .await
                    .map(Json::from)
                    .map_err(|error| {
                        tracing::error!(error = %error, "read agent can_act_as tuples failed");
                        HandlerError::from(TerminalError::new_with_code(
                            503,
                            "authorization engine unavailable",
                        ))
                    })
            })
            .name("agents_deactivate_read_can_act_as")
            .await?
            .into_inner();
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                agent_admin::deactivate_agent(pool, identity, agent_id, can_act_as).await
            })
            .name("agents_deactivate")
            .await?)
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { agent_admin::grant_can_act_as(pool, identity, request).await })
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
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move { agent_admin::revoke_can_act_as(pool, identity, request).await })
            .name("agents_revoke_can_act_as")
            .await?)
    }
}

fn validate_agent_name(name: &str) -> Result<(), HandlerError> {
    if name.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "agent name is required").into());
    }
    Ok(())
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
        Relation::Operator,
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
        IdentityType::Agent | IdentityType::Service | IdentityType::Contact => {
            identity.acting_on_behalf_of
        }
    }
}
