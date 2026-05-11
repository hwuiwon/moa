//! Restate service for first-class agent registration.

use moa_authz::{enqueue_raw, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Agent registration request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAgentRequest {
    /// Tenant the new agent belongs to.
    pub tenant_id: Uuid,
    /// Optional template used to instantiate this agent.
    pub template_id: Option<Uuid>,
}

/// Agent registration response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterAgentResponse {
    /// Newly registered agent ID.
    pub agent_id: Uuid,
}

/// Restate service surface for registering first-class agent principals.
#[restate_sdk::service]
#[name = "AgentRegistry"]
pub trait AgentRegistry {
    /// Registers a first-class agent principal.
    async fn register_agent(
        request: Json<RegisterAgentRequest>,
    ) -> Result<Json<RegisterAgentResponse>, HandlerError>;
}

/// Concrete agent-registry service implementation.
#[derive(Clone, Default)]
pub struct AgentRegistryImpl;

impl AgentRegistry for AgentRegistryImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn register_agent(
        &self,
        ctx: Context<'_>,
        request: Json<RegisterAgentRequest>,
    ) -> Result<Json<RegisterAgentResponse>, HandlerError> {
        annotate_restate_handler_span("AgentRegistry", "register_agent");
        let request = request.into_inner();
        let identity = require_identity(&ctx)?;
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            request.tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;

        let pool = OrchestratorCtx::current().graph_pool.clone();
        Ok(ctx
            .run(|| async move {
                register_agent_inner(pool, request, identity)
                    .await
                    .map(Json::from)
            })
            .name("register_agent")
            .await?)
    }
}

async fn register_agent_inner(
    pool: PgPool,
    request: RegisterAgentRequest,
    identity: Identity,
) -> Result<RegisterAgentResponse, HandlerError> {
    let agent_id = Uuid::now_v7();
    let operator_user_id = operator_user_id(&identity)?;
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

    sqlx::query(
        r#"
        INSERT INTO agents (id, tenant_id, template_id, operator_user_id)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(agent_id)
    .bind(request.tenant_id)
    .bind(request.template_id)
    .bind(operator_user_id)
    .execute(&mut *transaction)
    .await
    .map_err(|error| TerminalError::new(format!("insert agent: {error}")))?;

    enqueue_raw(
        &mut *transaction,
        TupleOp::Write,
        &format!("tenant:{}", request.tenant_id),
        "tenant",
        &format!("agent:{agent_id}"),
        Some(request.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox agent tenant tuple: {error}")))?;

    enqueue_raw(
        &mut *transaction,
        TupleOp::Write,
        &format!("user:{operator_user_id}"),
        "operator",
        &format!("agent:{agent_id}"),
        Some(request.tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox agent operator tuple: {error}")))?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    Ok(RegisterAgentResponse { agent_id })
}

fn operator_user_id(identity: &Identity) -> Result<Uuid, HandlerError> {
    if let Some(user_id) = identity.acting_on_behalf_of {
        return Ok(user_id);
    }

    match identity.identity_type {
        IdentityType::User => Ok(identity.id),
        IdentityType::Agent | IdentityType::Service => Err(TerminalError::new_with_code(
            403,
            "agent registration requires a user operator",
        )
        .into()),
    }
}
