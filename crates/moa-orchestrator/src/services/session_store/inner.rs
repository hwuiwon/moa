//! Backend calls used by Restate session-store handlers.

use super::*;
use moa_agents::AgentResolver;
use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::{
    ActionRuleScope, AgentContext, AgentSessionSelection, ModelId,
    traits::{Identity, IdentityType},
};

/// Creates a session row and enqueues the authorization tuples needed by its first caller.
pub(crate) async fn create_session_for_identity(
    store: &PostgresSessionStore,
    pool: &sqlx::PgPool,
    meta: SessionMeta,
    identity: Identity,
) -> Result<SessionId, HandlerError> {
    let (owner_user_type, owner_id) = owner_tuple_subject(&identity)?;
    let tenant_id = meta.tenant_id;
    let contact_id = meta.contact.as_ref().map(|contact| contact.contact_id);
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

    let session_id = store
        .create_session_in_tx(&mut transaction, meta)
        .await
        .map_err(HandlerError::from)?;

    let owner_tuple = TupleKey::new(
        owner_user_type,
        owner_id,
        Relation::Owner,
        ObjectType::Session,
        session_id.0,
    );
    enqueue(
        &mut *transaction,
        TupleOp::Write,
        &owner_tuple,
        Some(tenant_id.0),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox owner tuple: {error}")))?;

    enqueue_raw(
        &mut *transaction,
        TupleOp::Write,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &format!("session:{session_id}"),
        Some(tenant_id.0),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox tenant tuple: {error}")))?;

    if let Some(contact_id) = contact_id {
        enqueue_raw(
            &mut *transaction,
            TupleOp::Write,
            &format!("contact:{contact_id}"),
            "contact",
            &format!("session:{session_id}"),
            Some(tenant_id.0),
        )
        .await
        .map_err(|error| TerminalError::new(format!("authz outbox contact tuple: {error}")))?;
    }

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    // The active-session gauge is refreshed off the write path by the store's
    // background timer, so session creation does not run a COUNT(*) here.

    Ok(session_id)
}

/// Creates a session after resolving and pinning a tenant-configured agent policy.
pub(crate) async fn create_agent_session_for_identity(
    store: &PostgresSessionStore,
    pool: sqlx::PgPool,
    request: CreateAgentSessionRequest,
    identity: Identity,
) -> Result<CreateAgentSessionResponse, HandlerError> {
    let mut meta = request.meta;
    if meta
        .agent_context
        .as_ref()
        .is_some_and(|context| !context.is_system_default())
    {
        return Err(
            TerminalError::new("create_agent_session resolves agent_context server-side").into(),
        );
    }

    let agent_context =
        resolve_agent_context_for_session(pool.clone(), &meta, &request.agent).await?;
    meta.agent_context = Some(agent_context.clone());
    apply_agent_model_policy(&mut meta, &agent_context)?;
    let session_id = create_session_for_identity(store, &pool, meta, identity).await?;

    Ok(CreateAgentSessionResponse {
        session_id,
        agent_context,
    })
}

/// Resolves the agent selection for a session and returns the pinned runtime context.
pub(crate) async fn resolve_agent_context_for_session(
    pool: sqlx::PgPool,
    meta: &SessionMeta,
    agent: &AgentSessionSelection,
) -> Result<AgentContext, HandlerError> {
    if meta
        .agent_context
        .as_ref()
        .is_some_and(|context| !context.is_system_default())
    {
        return Err(TerminalError::new("agent_context is resolved server-side").into());
    }

    let scope = session_agent_scope(meta);
    let selected_agent_count =
        usize::from(agent.installation_uid.is_some()) + usize::from(agent.revision_uid.is_some());
    if selected_agent_count != 1 {
        return Err(TerminalError::new(
            "create_agent_session requires exactly one agent installation_uid or revision_uid",
        )
        .into());
    }

    let resolver = AgentResolver::new(pool);
    let policy = match (agent.installation_uid, agent.revision_uid) {
        (Some(installation_uid), None) => resolver
            .resolve_installation(&scope, installation_uid)
            .await
            .map_err(HandlerError::from)?,
        (None, Some(revision_uid)) => resolver
            .resolve_exact_revision(&scope, revision_uid)
            .await
            .map_err(HandlerError::from)?,
        _ => unreachable!("agent selection cardinality checked above"),
    };
    Ok(policy.agent_context)
}

/// Applies the pinned agent model policy to a session being admitted.
pub(crate) fn apply_agent_model_policy(
    meta: &mut SessionMeta,
    agent_context: &AgentContext,
) -> Result<(), HandlerError> {
    let snapshot = agent_context
        .parsed_policy_snapshot()
        .map_err(HandlerError::from)?;
    let model_policy = snapshot.model_policy;
    if meta.model.as_str().trim().is_empty()
        && let Some(default_model) = model_policy.default_model.as_deref()
    {
        meta.model = ModelId::new(default_model);
    }

    if model_policy.allowed_models.is_empty()
        || model_policy
            .allowed_models
            .iter()
            .any(|model| model == meta.model.as_str())
    {
        return Ok(());
    }

    let fallback = model_policy
        .fallback_model
        .as_deref()
        .or(model_policy.default_model.as_deref())
        .filter(|candidate| {
            model_policy
                .allowed_models
                .iter()
                .any(|model| model == *candidate)
        });
    if let Some(fallback) = fallback {
        meta.model = ModelId::new(fallback);
        return Ok(());
    }

    Err(TerminalError::new(format!(
        "agent policy {} for {} does not allow model {}",
        agent_context.policy_hash, agent_context.definition_ref, meta.model
    ))
    .into())
}

fn session_agent_scope(meta: &SessionMeta) -> ActionRuleScope {
    ActionRuleScope::Tenant {
        tenant_id: meta.tenant_id,
    }
}

impl SessionStoreImpl {
    #[cfg(test)]
    pub(super) async fn create_session_inner(
        &self,
        meta: SessionMeta,
    ) -> Result<SessionId, HandlerError> {
        self.store
            .create_session(meta)
            .await
            .map_err(HandlerError::from)
    }
}

fn owner_tuple_subject(identity: &Identity) -> Result<(UserType, uuid::Uuid), HandlerError> {
    if let Some(api_key_id) = identity.api_key_id {
        return Ok((UserType::ApiKey, api_key_id));
    }

    match identity.identity_type {
        IdentityType::User => Ok((UserType::User, identity.id)),
        IdentityType::Contact => Ok((UserType::Contact, identity.id)),
        IdentityType::Agent => Ok((UserType::Agent, identity.id)),
        IdentityType::Service => {
            Err(TerminalError::new_with_code(403, "service identities cannot own sessions").into())
        }
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        AgentContext, AgentModelPolicy, AgentPolicySnapshot, ModelId,
        SYSTEM_DEFAULT_AGENT_POLICY_HASH, SYSTEM_DEFAULT_AGENT_REF,
        SYSTEM_DEFAULT_AGENT_REVISION_UID, SessionMeta,
    };

    use super::apply_agent_model_policy;

    #[test]
    fn agent_model_policy_fills_empty_model() {
        // Pins: configured-agent sessions inherit the agent default model before persistence.
        let mut meta = SessionMeta {
            model: ModelId::new(""),
            ..SessionMeta::default()
        };
        let context = agent_context(AgentModelPolicy {
            default_model: Some("claude-sonnet-4-6".to_string()),
            allowed_models: vec!["claude-sonnet-4-6".to_string()],
            fallback_model: None,
        });

        apply_agent_model_policy(&mut meta, &context).expect("default model should apply");

        assert_eq!(meta.model.as_str(), "claude-sonnet-4-6");
    }

    #[test]
    fn agent_model_policy_uses_valid_fallback_for_disallowed_model() {
        // Pins: disallowed caller model falls back only when the fallback is explicitly allowed.
        let mut meta = SessionMeta {
            model: ModelId::new("gpt-expensive"),
            ..SessionMeta::default()
        };
        let context = agent_context(AgentModelPolicy {
            default_model: Some("claude-haiku".to_string()),
            allowed_models: vec!["claude-haiku".to_string()],
            fallback_model: Some("claude-haiku".to_string()),
        });

        apply_agent_model_policy(&mut meta, &context).expect("valid fallback should apply");

        assert_eq!(meta.model.as_str(), "claude-haiku");
    }

    #[test]
    fn agent_model_policy_rejects_disallowed_model_without_valid_fallback() {
        // Pins: model policy is an admission gate, not only a UI hint.
        let mut meta = SessionMeta {
            model: ModelId::new("gpt-expensive"),
            ..SessionMeta::default()
        };
        let context = agent_context(AgentModelPolicy {
            default_model: None,
            allowed_models: vec!["claude-haiku".to_string()],
            fallback_model: Some("claude-opus".to_string()),
        });

        apply_agent_model_policy(&mut meta, &context)
            .expect_err("invalid model should be rejected");
    }

    fn agent_context(model_policy: AgentModelPolicy) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            model_policy,
            ..AgentPolicySnapshot::default()
        };
        AgentContext {
            agent_id: None,
            installation_uid: None,
            deployment_uid: None,
            definition_ref: SYSTEM_DEFAULT_AGENT_REF.to_string(),
            revision_uid: SYSTEM_DEFAULT_AGENT_REVISION_UID,
            policy_hash: SYSTEM_DEFAULT_AGENT_POLICY_HASH.to_string(),
            display_name: "Test Agent".to_string(),
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            policy_snapshot: serde_json::to_value(snapshot).expect("serialize snapshot"),
        }
    }
}
