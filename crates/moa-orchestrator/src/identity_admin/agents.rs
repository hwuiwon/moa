//! Agent principal lifecycle repository and application operations.

use moa_auth_providers::api_keys;
use moa_authz::{FgaTuple, enqueue_raw};
use moa_authz_schema::TupleOp;
use moa_core::traits::{Identity, IdentityType};
use moa_ocsf::ActorInput;
use moa_wire::agents::{AgentActAsRequest, AgentSummary, RegisterAgentRequest};
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::PgPool;
use uuid::Uuid;

/// Register an agent in the caller's tenant.
pub(crate) async fn register_agent(
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
    moa_ocsf::emit_agent_registered_tx(&mut transaction, identity.tenant_id.0, &identity, agent.id)
        .await
        .map_err(|error| TerminalError::new(format!("audit agent register: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(agent)
}

/// List active agents operated by the caller.
pub(crate) async fn list_agents(
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

/// Load one tenant-scoped agent.
pub(crate) async fn get_agent(
    pool: PgPool,
    identity: Identity,
    agent_id: Uuid,
) -> Result<AgentSummary, HandlerError> {
    load_agent(&pool, agent_id)
        .await
        .and_then(|agent| ensure_same_tenant(agent, identity.tenant_id.0))
}

/// Deactivate an agent and revoke its local API keys.
pub(crate) async fn deactivate_agent(
    pool: PgPool,
    identity: Identity,
    agent_id: Uuid,
    can_act_as: Vec<FgaTuple>,
) -> Result<(), HandlerError> {
    let agent = ensure_same_tenant(load_agent(&pool, agent_id).await?, identity.tenant_id.0)?;
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

/// Grant an agent the right to act as a user.
pub(crate) async fn grant_can_act_as(
    pool: PgPool,
    identity: Identity,
    request: AgentActAsRequest,
) -> Result<(), HandlerError> {
    mutate_can_act_as(pool, identity, request, TupleOp::Write).await
}

/// Revoke an agent's right to act as a user.
pub(crate) async fn revoke_can_act_as(
    pool: PgPool,
    identity: Identity,
    request: AgentActAsRequest,
) -> Result<(), HandlerError> {
    mutate_can_act_as(pool, identity, request, TupleOp::Delete).await
}

async fn mutate_can_act_as(
    pool: PgPool,
    identity: Identity,
    request: AgentActAsRequest,
    op: TupleOp,
) -> Result<(), HandlerError> {
    let agent = ensure_same_tenant(
        load_agent(&pool, request.agent_id).await?,
        identity.tenant_id.0,
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
        &format!("operator:{}", request.user_id),
        "can_act_as",
        &format!("agent:{}", request.agent_id),
        Some(identity.tenant_id.0),
    )
    .await
    .map_err(|error| TerminalError::new(format!("agent can_act_as outbox: {error}")))?;
    match op {
        TupleOp::Write => {
            moa_ocsf::emit_delegation_granted_tx(
                &mut transaction,
                identity.tenant_id.0,
                &identity,
                request.agent_id,
                request.user_id,
            )
            .await
        }
        TupleOp::Delete => {
            moa_ocsf::emit_delegation_revoked_tx(
                &mut transaction,
                identity.tenant_id.0,
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
            &format!("operator:{operator_user_id}"),
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

fn required_operator_user_id(identity: &Identity) -> Result<Uuid, HandlerError> {
    if let Some(user_id) = identity.acting_on_behalf_of {
        return Ok(user_id);
    }
    if identity.identity_type == IdentityType::Operator {
        return Ok(identity.id);
    }
    Err(TerminalError::new_with_code(403, "agent registration requires a user operator").into())
}

fn actor_user_id(identity: &Identity) -> Option<Uuid> {
    match identity.identity_type {
        IdentityType::Operator => Some(identity.id),
        IdentityType::Agent | IdentityType::Service | IdentityType::Contact => {
            identity.acting_on_behalf_of
        }
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
            tenant_id: moa_core::types::identifiers::TenantId::from(
                Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
                    .expect("tenant fixture UUID should parse"),
            ),
            api_key_id: None,
            acting_on_behalf_of,
        }
    }

    #[test]
    fn required_operator_user_id_accepts_user_identity() {
        // Pins: a direct human caller becomes the operator for newly registered agents.
        let user_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("user fixture UUID should parse");
        let identity = identity(IdentityType::Operator, user_id, None);

        let operator_user_id =
            required_operator_user_id(&identity).expect("operator identity should be accepted");

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
