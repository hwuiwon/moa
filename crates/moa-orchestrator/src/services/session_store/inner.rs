//! Backend calls used by Restate session-store handlers.

use super::*;
use moa_agents::AgentResolver;
use moa_authz::{enqueue, enqueue_raw};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::{
    AgentContext, AgentSessionSelection, MemoryScope, ModelId,
    traits::{Identity, IdentityType},
};

/// Creates a session row and enqueues the authorization tuples needed by its first caller.
pub(crate) async fn create_session_for_identity(
    store: &PostgresSessionStore,
    meta: SessionMeta,
    identity: Identity,
) -> Result<SessionId, HandlerError> {
    let (owner_user_type, owner_id) = owner_tuple_subject(&identity)?;
    let tenant_id = identity.tenant_id;
    let workspace_id = meta.workspace_id.clone();
    let mut transaction = store
        .pool()
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
        Some(tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox owner tuple: {error}")))?;

    enqueue_raw(
        &mut *transaction,
        TupleOp::Write,
        &format!("workspace:{workspace_id}"),
        "workspace",
        &format!("session:{session_id}"),
        Some(tenant_id),
    )
    .await
    .map_err(|error| TerminalError::new(format!("authz outbox parent tuple: {error}")))?;

    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    store
        .refresh_active_session_metric()
        .await
        .map_err(HandlerError::from)?;

    Ok(session_id)
}

/// Creates a session after resolving and pinning a tenant-configured agent policy.
pub(crate) async fn create_agent_session_for_identity(
    store: &PostgresSessionStore,
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

    let agent_context = resolve_agent_context_for_session(store, &meta, &request.agent).await?;
    meta.agent_context = Some(agent_context.clone());
    apply_agent_model_policy(&mut meta, &agent_context)?;
    let session_id = create_session_for_identity(store, meta, identity).await?;

    Ok(CreateAgentSessionResponse {
        session_id,
        agent_context,
    })
}

/// Resolves the agent selection for a session and returns the pinned runtime context.
pub(crate) async fn resolve_agent_context_for_session(
    store: &PostgresSessionStore,
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

    let resolver = AgentResolver::new(store.pool().clone());
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

fn session_agent_scope(meta: &SessionMeta) -> MemoryScope {
    if meta.user_id.as_str().is_empty() {
        MemoryScope::Workspace {
            workspace_id: meta.workspace_id.clone(),
        }
    } else {
        MemoryScope::User {
            workspace_id: meta.workspace_id.clone(),
            user_id: meta.user_id.clone(),
        }
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

    pub(super) async fn append_event_inner(
        &self,
        request: AppendEventRequest,
    ) -> Result<u64, HandlerError> {
        if matches!(&request.event, Event::Error { .. }) {
            record_session_error("event_log");
        }
        self.store
            .emit_event(request.session_id, request.event)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_events_inner(
        &self,
        request: GetEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .get_events(request.session_id, request.range)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_session_inner(
        &self,
        session_id: SessionId,
    ) -> Result<SessionMeta, HandlerError> {
        self.store
            .get_session(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_status_inner(
        &self,
        request: UpdateStatusRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_status(request.session_id, request.status)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn search_events_inner(
        &self,
        request: SearchEventsRequest,
    ) -> Result<Vec<EventRecord>, HandlerError> {
        self.store
            .search_events(&request.query, request.filter)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_sessions_inner(
        &self,
        request: ListSessionsRequest,
    ) -> Result<Vec<SessionSummary>, HandlerError> {
        self.store
            .list_sessions(request.filter)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn workspace_cost_since_inner(
        &self,
        request: WorkspaceCostSinceRequest,
    ) -> Result<u32, HandlerError> {
        self.store
            .workspace_cost_since(&request.workspace_id, request.since)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn create_segment_inner(
        &self,
        request: CreateSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .create_segment(&request.segment)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn complete_segment_inner(
        &self,
        request: CompleteSegmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .complete_segment(request.segment_id, request.update)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_active_segment_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Option<TaskSegment>, HandlerError> {
        self.store
            .get_active_segment(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_segments_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TaskSegment>, HandlerError> {
        self.store
            .list_segments(session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_segment_assessment_inner(
        &self,
        request: UpdateSegmentAssessmentRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_segment_assessment(request.segment_id, &request.assessment)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_segment_baseline_inner(
        &self,
        request: GetSegmentBaselineRequest,
    ) -> Result<Option<SegmentBaseline>, HandlerError> {
        self.store
            .get_segment_baseline(&request.tenant_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_skill_resolution_rates_inner(
        &self,
        request: ListSkillResolutionRatesRequest,
    ) -> Result<Vec<SkillResolutionRate>, HandlerError> {
        self.store
            .list_skill_resolution_rates(&request.tenant_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_task_strategy_success_rates_inner(
        &self,
        request: ListTaskStrategySuccessRatesRequest,
    ) -> Result<Vec<TaskStrategySuccessRate>, HandlerError> {
        self.store
            .list_task_strategy_success_rates(&request.tenant_id, &request.task_fingerprint)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn append_experience_record_inner(
        &self,
        request: AppendExperienceRecordRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .append_experience_record(&request.experience)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_experience_records_inner(
        &self,
        request: ListExperienceRecordsRequest,
    ) -> Result<Vec<ExperienceRecord>, HandlerError> {
        self.store
            .list_experience_records(request.session_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn append_experience_attributions_inner(
        &self,
        request: AppendExperienceAttributionsRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .append_experience_attributions(&request.attributions)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn list_experience_attributions_inner(
        &self,
        request: ListExperienceAttributionsRequest,
    ) -> Result<Vec<ExperienceAttribution>, HandlerError> {
        self.store
            .list_experience_attributions(request.experience_id)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn append_learning_candidate_inner(
        &self,
        request: AppendLearningCandidateRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .append_learning_candidate(&request.candidate)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn get_learning_candidate_inner(
        &self,
        request: GetLearningCandidateRequest,
    ) -> Result<LearningCandidate, HandlerError> {
        self.store
            .get_learning_candidate(&request.workspace_id, request.candidate_id)
            .await
            .map_err(HandlerError::from)?
            .ok_or_else(|| {
                TerminalError::new_with_code(
                    404,
                    format!(
                        "learning candidate {} not found in workspace {}",
                        request.candidate_id, request.workspace_id
                    ),
                )
                .into()
            })
    }

    pub(super) async fn list_learning_candidates_inner(
        &self,
        request: ListLearningCandidatesRequest,
    ) -> Result<Vec<LearningCandidate>, HandlerError> {
        self.store
            .list_learning_candidates(&request.tenant_id, request.status, request.limit)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn update_learning_candidate_status_inner(
        &self,
        request: UpdateLearningCandidateStatusRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .update_learning_candidate_status(&request.update)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn refresh_segment_materialized_views_inner(
        &self,
    ) -> Result<(), HandlerError> {
        self.store
            .refresh_segment_materialized_views()
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_tool_use_inner(
        &self,
        request: RecordSegmentToolUseRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_tool_use(request.session_id, &request.tool_name)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_skill_activation_inner(
        &self,
        request: RecordSegmentSkillActivationRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_skill_activation(request.session_id, &request.skill_name)
            .await
            .map_err(HandlerError::from)
    }

    pub(super) async fn record_segment_turn_usage_inner(
        &self,
        request: RecordSegmentTurnUsageRequest,
    ) -> Result<(), HandlerError> {
        self.store
            .record_active_segment_turn_usage(request.session_id, request.token_cost)
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
