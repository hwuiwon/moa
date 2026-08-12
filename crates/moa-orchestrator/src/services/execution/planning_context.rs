//! Immutable execution planning-context assembly.

use super::capability_catalog::{
    build_capability_response, load_connection_refs, load_locked_skill_revisions,
    load_serving_revisions,
};
use super::support::{execution_error, execution_scope, invalid_execution_request};
use super::*;

/// Complete bounded input for one immutable planning-context assembly.
pub(super) struct PlanningContextInput {
    /// Shared runtime database pool.
    pub(super) pool: sqlx::PgPool,
    /// Session-scoped capability registrations.
    pub(super) registrations: Vec<(ToolDefinition, ToolExecution)>,
    /// Validated execution policy.
    pub(super) config: ExecutionConfig,
    /// Authoritative parent session metadata.
    pub(super) parent: moa_core::types::session::SessionMeta,
    /// Effective user that owns the planning request.
    pub(super) owner_user_id: moa_core::types::identifiers::UserId,
    /// Exact persisted user event that originated planning.
    pub(super) originating_event: Event,
    /// Durable admission timestamp from the originating event.
    pub(super) planning_admitted_at: chrono::DateTime<Utc>,
    /// Caller request already authorized by the service boundary.
    pub(super) request: ExecutionPlanningContextRequest,
}

/// Builds and persists one caller-authorized immutable planning context.
pub(super) async fn planning_context_inner(
    input: PlanningContextInput,
) -> Result<ExecutionPlanningContextResponse, HandlerError> {
    let PlanningContextInput {
        pool,
        registrations,
        config,
        parent,
        owner_user_id,
        originating_event,
        planning_admitted_at,
        request,
    } = input;
    let registrations = registrations
        .into_iter()
        .filter_map(|registration| {
            if matches!(
                &registration.1,
                ToolExecution::InstalledConnectorAction { .. }
            ) {
                // Installed actions are selected by the exact connector bindings
                // in the session action policy. A generated connection-qualified
                // model name is not an authored raw-tool dependency and must not
                // become a second source of authority.
                return Some(Ok(registration));
            }
            let allowed = parent
                .agent_context
                .as_ref()
                .map(|context| context.allows_tool(&registration.0.name))
                .transpose();
            match allowed {
                Ok(Some(false)) => None,
                Ok(Some(true) | None) => Some(Ok(registration)),
                Err(error) => Some(Err(invalid_execution_request(format!(
                    "invalid session tool policy: {error}"
                )))),
            }
        })
        .collect::<Result<Vec<_>, HandlerError>>()?;
    let scope = request.contact_id.map_or(
        ActionRuleScope::Tenant {
            tenant_id: request.tenant_id,
        },
        |contact_id| ActionRuleScope::Contact {
            tenant_id: request.tenant_id,
            contact_id,
        },
    );
    let registry = ArtifactRegistry::new(pool.clone());
    let revisions = load_serving_revisions(&registry, &scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let skill_policy = parent
        .agent_context
        .as_ref()
        .map(|context| context.parsed_policy_snapshot())
        .transpose()
        .map_err(|error| {
            invalid_execution_request(format!("invalid session skill policy: {error}"))
        })?
        .map(|snapshot| snapshot.skill_policy)
        .unwrap_or_default();
    let locked_skill_revisions =
        load_locked_skill_revisions(&registry, &scope, parent.agent_context.as_ref()).await?;
    let skill_context = build_planning_skill_context(
        revisions,
        locked_skill_revisions,
        &skill_policy,
        request.requested_template.as_ref(),
    )
    .map_err(invalid_execution_request)?;
    let connection_refs = load_connection_refs(pool.clone(), request.tenant_id)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let capability_response =
        build_capability_response(&registrations, &skill_context.revisions, &connection_refs)
            .map_err(execution_error)?;

    let pinned_instruction_skills = skill_context.pinned_instruction_skills;
    let execution_templates = skill_context.execution_templates;

    let skill_refs = pinned_instruction_skills
        .iter()
        .map(|skill| skill.skill_ref.clone())
        .collect::<Vec<_>>();
    let authorization = moa_execution::ExecutionAuthorizationEnvelope {
        capability_refs: capability_response
            .catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect(),
        skill_refs,
    };
    let event_hash = originating_user_event_hash(
        request.session_id,
        request.originating_user_sequence_num,
        &originating_event,
    )
    .map_err(execution_error)?;
    let deadline_at = capped_planning_deadline(
        planning_admitted_at,
        request.deadline_at,
        config.maximum_horizon_seconds,
    )
    .map_err(invalid_execution_request)?;
    let snapshot = ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id: request.tenant_id,
        contact_id: request.contact_id,
        session_id: request.session_id,
        originating_user_sequence_num: request.originating_user_sequence_num,
        originating_user_event_hash: event_hash.to_string(),
        owner_user_id,
        catalog: capability_response.catalog,
        authorization,
        pinned_instruction_skills,
        execution_templates,
        budget: ExecutionBudgetLimit {
            max_cost_microusd: Some(config.max_cost_microusd),
            max_tokens: Some(config.max_tokens),
            max_tasks: Some(config.max_tasks),
            max_tool_calls: Some(config.max_tool_calls),
            max_retrieved_bytes: Some(config.max_retrieved_bytes),
            deadline_at: Some(deadline_at),
        },
    };
    let hash = planning_context_hash(&snapshot).map_err(execution_error)?;
    let repository = ExecutionRepository::new(pool);
    let scope = execution_scope(request.tenant_id, request.contact_id);
    match repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot,
                planning_context_hash: hash,
            },
        )
        .await
        .map_err(execution_error)?
    {
        PlanningContextWriteOutcome::Created(record) => Ok(ExecutionPlanningContextResponse {
            planning_context_uid: record.planning_context_uid,
            planning_context_hash: record.planning_context_hash.to_string(),
            snapshot: record.snapshot,
            created: true,
        }),
        PlanningContextWriteOutcome::Replayed(record) => Ok(ExecutionPlanningContextResponse {
            planning_context_uid: record.planning_context_uid,
            planning_context_hash: record.planning_context_hash.to_string(),
            snapshot: record.snapshot,
            created: false,
        }),
        PlanningContextWriteOutcome::Conflict => Err(TerminalError::new_with_code(
            409,
            "originating user event already has a different planning context",
        )
        .into()),
    }
}

/// Returns the admitted absolute deadline bounded by caller authority and configured horizon.
pub(super) fn capped_planning_deadline(
    planning_admitted_at: chrono::DateTime<Utc>,
    authorized_deadline_at: chrono::DateTime<Utc>,
    maximum_horizon_seconds: u64,
) -> Result<chrono::DateTime<Utc>, &'static str> {
    let horizon_seconds = i64::try_from(maximum_horizon_seconds)
        .map_err(|_| "execution maximum horizon does not fit the timestamp range")?;
    let horizon = chrono::TimeDelta::try_seconds(horizon_seconds)
        .ok_or("execution maximum horizon does not fit the timestamp range")?;
    let maximum_deadline = planning_admitted_at
        .checked_add_signed(horizon)
        .ok_or("execution maximum horizon exceeds the timestamp range")?;
    let deadline_at = authorized_deadline_at.min(maximum_deadline);
    if deadline_at <= planning_admitted_at {
        return Err("execution planning deadline must be later than admission time");
    }
    Ok(deadline_at)
}

#[derive(Debug)]
pub(super) struct PlanningSkillContext {
    pub(super) revisions: Vec<StoredArtifactRevision>,
    pub(super) pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    pub(super) execution_templates: Vec<PinnedExecutionTemplate>,
}

pub(super) fn build_planning_skill_context(
    revisions: Vec<StoredArtifactRevision>,
    locked_skill_revisions: Vec<StoredArtifactRevision>,
    policy: &AgentSkillPolicy,
    requested_template: Option<&moa_core::types::execution_planning::PinnedExecutionTemplateRef>,
) -> Result<PlanningSkillContext, String> {
    let policy_refs = policy
        .refs
        .iter()
        .map(|reference| canonical_skill_policy_ref(reference))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut non_skill_revisions = Vec::new();
    let mut resolved_skills = BTreeMap::new();
    for revision in revisions {
        if !matches!(revision.document.definition, ArtifactDefinition::Skill(_)) {
            non_skill_revisions.push(revision);
            continue;
        }
        let reference = skill_revision_ref(&revision)?;
        if let Some(previous) = resolved_skills.insert(reference.clone(), revision) {
            let duplicate_uid = resolved_skills
                .get(&reference)
                .map(|current| current.revision_uid == previous.revision_uid)
                .unwrap_or(false);
            return Err(if duplicate_uid {
                format!(
                    "duplicate exact skill revision: {reference}@{}",
                    previous.revision_uid
                )
            } else {
                format!("multiple serving revisions for planning skill: {reference}")
            });
        }
    }

    let mut locked_skills = BTreeMap::new();
    for revision in locked_skill_revisions {
        let reference = skill_revision_ref(&revision)?;
        if let Some(previous) = locked_skills.insert(reference.clone(), revision) {
            let duplicate_uid = locked_skills
                .get(&reference)
                .map(|current| current.revision_uid == previous.revision_uid)
                .unwrap_or(false);
            return Err(if duplicate_uid {
                format!(
                    "duplicate exact locked skill revision: {reference}@{}",
                    previous.revision_uid
                )
            } else {
                format!("multiple locked revisions for planning skill: {reference}")
            });
        }
    }
    let mut ordered = resolved_skills.into_iter().collect::<Vec<_>>();
    match policy.mode {
        moa_core::types::agent::AgentSkillPolicyMode::Auto => {}
        moa_core::types::agent::AgentSkillPolicyMode::Allowlist => {
            ordered.retain(|(reference, _)| policy_refs.contains(reference));
        }
        moa_core::types::agent::AgentSkillPolicyMode::Denylist => {
            ordered.retain(|(reference, _)| !policy_refs.contains(reference));
        }
        moa_core::types::agent::AgentSkillPolicyMode::Pinned => {
            ordered.sort_by_key(|(reference, _)| {
                (!policy_refs.contains(reference), reference.clone())
            });
        }
    }
    if let Some(max_visible) = policy.max_visible {
        let limit = usize::try_from(max_visible)
            .map_err(|_| "agent skill max_visible does not fit usize".to_string())?;
        ordered.truncate(limit);
    }
    for (reference, revision) in &mut ordered {
        if let Some(locked) = locked_skills.remove(reference) {
            *revision = locked;
        }
    }
    ordered.sort_by(|(left_ref, left), (right_ref, right)| {
        left_ref
            .cmp(right_ref)
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    let selected_skills = ordered
        .into_iter()
        .map(|(_, revision)| revision)
        .collect::<Vec<_>>();
    let mut pinned_instruction_skills = Vec::new();
    let mut execution_templates = Vec::new();
    for revision in &selected_skills {
        let ArtifactDefinition::Skill(skill) = &revision.document.definition else {
            continue;
        };
        let skill_ref = ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone());
        pinned_instruction_skills.push(PinnedInstructionSkill {
            skill_ref: skill_ref.clone(),
            revision_uid: revision.revision_uid,
        });
        if let Some(execution_plan) = &skill.execution_plan {
            execution_templates.push(PinnedExecutionTemplate {
                skill_ref,
                revision_uid: revision.revision_uid,
                skill_input_schema: skill.inputs.clone(),
                execution_plan: execution_plan.clone(),
            });
        }
    }
    pinned_instruction_skills.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
    execution_templates.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });

    if let Some(requested) = requested_template {
        let parsed = requested
            .skill_ref
            .parse::<ArtifactRef>()
            .map_err(|error| format!("invalid execution template ref: {error}"))?;
        if parsed.to_string() != requested.skill_ref
            || execution_templates
                .iter()
                .filter(|template| {
                    template.skill_ref == parsed && template.revision_uid == requested.revision_uid
                })
                .count()
                != 1
        {
            return Err(
                "requested execution template is not an exact permitted pinned activated revision"
                    .to_string(),
            );
        }
    }

    non_skill_revisions.extend(selected_skills);

    Ok(PlanningSkillContext {
        revisions: non_skill_revisions,
        pinned_instruction_skills,
        execution_templates,
    })
}

pub(super) fn canonical_skill_policy_ref(reference: &str) -> Result<String, String> {
    let parsed = reference
        .parse::<ArtifactRef>()
        .map_err(|error| format!("invalid agent skill policy ref `{reference}`: {error}"))?;
    if !matches!(
        parsed,
        ArtifactRef::Artifact {
            kind: ArtifactKind::Skill,
            ..
        }
    ) || parsed.to_string() != reference
    {
        return Err(format!(
            "agent skill policy ref must be canonical skill:// reference: {reference}"
        ));
    }
    Ok(reference.to_string())
}

pub(super) fn skill_revision_ref(revision: &StoredArtifactRevision) -> Result<String, String> {
    if !matches!(
        revision.status,
        ArtifactStatus::Ready | ArtifactStatus::Superseded
    ) || !matches!(revision.document.definition, ArtifactDefinition::Skill(_))
    {
        return Err(format!(
            "planning skill revision {} is {} and is not executable exact-pinned skill content",
            revision.revision_uid, revision.status
        ));
    }
    ArtifactRef::artifact(ArtifactKind::Skill, revision.name.clone())
        .canonical_string()
        .map_err(|error| format!("invalid planning skill revision ref: {error}"))
}
