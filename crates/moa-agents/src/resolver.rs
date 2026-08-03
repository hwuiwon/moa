//! Resolver for installed and exact configured-agent revisions.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::agent::{
    ActionPolicy, AgentDefinition, GuardrailPolicy, GuardrailStagePolicy, KnowledgeScopeMode,
    ModelPolicy, SkillPolicy, SkillPolicyMode, ToolPolicyMode,
};
use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, ArtifactScopeParts, StoredArtifactRevision};
use moa_artifacts::release::{EvalOverlayBinding, ReleaseState};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::agent::AgentActionPolicy, types::agent::AgentConnectorBinding,
    types::agent::AgentContext, types::agent::AgentKnowledgePolicy,
    types::agent::AgentKnowledgeScopeMode, types::agent::AgentModelPolicy,
    types::agent::AgentPolicySnapshot, types::agent::AgentRevisionLock,
    types::agent::AgentSandboxPolicy, types::agent::AgentSkillPolicy,
    types::agent::AgentSkillPolicyMode, types::agent::AgentToolPolicy,
    types::agent::AgentToolPolicyMode, types::agent::LockedToolRef,
    types::agent::ResolvedArtifactRevisionRef, types::guardrails::AgentGuardrailPolicy,
    types::guardrails::AgentGuardrailStagePolicy, types::identifiers::ModelId,
};
use moa_db::ScopedConn;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

use crate::definition::{AgentInstallationBinding, AgentInstallationPointer};
use crate::policy::AgentRuntimePolicy;

/// Postgres-backed configured-agent resolver.
pub struct AgentResolver {
    pool: PgPool,
}

impl AgentResolver {
    /// Creates an agent resolver over a shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolves an active installation pointer to a pinned runtime policy.
    pub async fn resolve_installation(
        &self,
        scope: &ActionRuleScope,
        installation_uid: Uuid,
    ) -> Result<AgentRuntimePolicy> {
        let pointer = self
            .load_installation_pointer(scope, installation_uid)
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(format!(
                    "agent installation {installation_uid} not found or not visible"
                ))
            })?;
        self.resolve_revision_with_pointer(scope, pointer, None)
            .await
    }

    /// Resolves an active installation while substituting release-evaluation revisions.
    ///
    /// The overlay is evaluation-only. Supplying one recomputes the dependency
    /// lock from the installed agent revision instead of reusing the production
    /// deployment lock, then the resulting exact lock is persisted on the eval
    /// session like any other agent context.
    pub async fn resolve_installation_with_overlay(
        &self,
        scope: &ActionRuleScope,
        installation_uid: Uuid,
        overlay: &EvalOverlayBinding,
    ) -> Result<AgentRuntimePolicy> {
        let pointer = self
            .load_installation_pointer(scope, installation_uid)
            .await?
            .ok_or_else(|| {
                MoaError::StorageError(format!(
                    "agent installation {installation_uid} not found or not visible"
                ))
            })?;
        self.resolve_revision_with_pointer(scope, pointer, Some(overlay))
            .await
    }

    /// Resolves an exact executable agent revision without moving any deployment pointer.
    pub async fn resolve_exact_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<AgentRuntimePolicy> {
        let revision = load_executable_agent_revision(&self.pool, scope, revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, None, None)
            .await
    }

    /// Resolves an exact agent revision with evaluation-only dependency substitutions.
    pub async fn resolve_exact_revision_with_overlay(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
        overlay: &EvalOverlayBinding,
    ) -> Result<AgentRuntimePolicy> {
        let revision = load_agent_revision_for_evaluation(&self.pool, scope, revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, None, Some(overlay))
            .await
    }

    /// Resolves a release-gated agent candidate into the exact lock activation persists.
    ///
    /// Unlike normal exact resolution, this accepts non-serving release states. It
    /// does not make the revision visible to a production session; it only builds
    /// the immutable deployment lock consumed by the attested activation path.
    pub async fn resolve_release_candidate(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<AgentRuntimePolicy> {
        let revision = load_agent_release_candidate(&self.pool, scope, revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, None, None)
            .await
    }

    /// Loads the immutable authorization binding for one active installation.
    ///
    /// Deployment callers authorize this binding before entering their write
    /// transaction, then compare it with the locked row before moving the
    /// deployment pointer.
    pub async fn load_installation_binding(
        &self,
        scope: &ActionRuleScope,
        installation_uid: Uuid,
    ) -> Result<Option<AgentInstallationBinding>> {
        let mut conn = scoped_conn_for_artifact_scope(&self.pool, scope).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            SELECT installation_uid, agent_id
            FROM moa.agent_installation
            WHERE installation_uid = $3
              AND status <> 'retired'
              AND storage_partition_id = $1
              AND (user_id IS NULL OR user_id = $2)
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(installation_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.map(|row| {
            Ok(AgentInstallationBinding {
                installation_uid: row.try_get("installation_uid").map_err(map_sqlx_error)?,
                agent_id: row.try_get("agent_id").map_err(map_sqlx_error)?,
            })
        })
        .transpose()
    }

    async fn resolve_revision_with_pointer(
        &self,
        scope: &ActionRuleScope,
        pointer: AgentInstallationPointer,
        overlay: Option<&EvalOverlayBinding>,
    ) -> Result<AgentRuntimePolicy> {
        let revision =
            load_executable_agent_revision(&self.pool, scope, pointer.current_revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, Some(pointer), overlay)
            .await
    }

    async fn resolve_loaded_revision(
        &self,
        scope: &ActionRuleScope,
        revision: StoredArtifactRevision,
        pointer: Option<AgentInstallationPointer>,
        overlay: Option<&EvalOverlayBinding>,
    ) -> Result<AgentRuntimePolicy> {
        let definition = agent_definition(&revision)?;
        let tool_policy = tool_policy_from_definition(definition);
        let model_policy = model_policy_from_definition(&definition.model_policy);
        let knowledge_policy = knowledge_policy_from_definition(definition);
        knowledge_policy.validate()?;
        let mut skill_policy = skill_policy_from_definition(&definition.skill_policy);
        let mut action_policy = action_policy_from_definition(&definition.action_policy);
        let release_target = match overlay {
            Some(overlay) => ArtifactRegistry::new(self.pool.clone())
                .load_release_overlay_target(scope, overlay)
                .await?
                .ok_or_else(|| {
                    MoaError::ValidationError(format!(
                        "release overlay {} did not resolve its exact target",
                        overlay.overlay_uid
                    ))
                })?,
            None => revision.clone(),
        };
        let release_target_ref = match (overlay, &release_target.kind) {
            (Some(_), ArtifactKind::Skill) => {
                let artifact_ref = ArtifactRef::artifact(ArtifactKind::Skill, &release_target.name);
                skill_policy.mode = AgentSkillPolicyMode::Pinned;
                skill_policy.refs.push(artifact_ref.to_string());
                skill_policy.refs.sort();
                skill_policy.refs.dedup();
                Some(artifact_ref)
            }
            (Some(_), ArtifactKind::Action) => {
                let artifact_ref = ArtifactRef::action_artifact(&release_target.name);
                action_policy.allowed.push(artifact_ref.to_string());
                action_policy.allowed.sort();
                action_policy.allowed.dedup();
                Some(artifact_ref)
            }
            (Some(_), ArtifactKind::Agent)
                if release_target.revision_uid == revision.revision_uid =>
            {
                None
            }
            (Some(_), kind) => {
                return Err(MoaError::ValidationError(format!(
                    "release overlay target {} is {kind}, which cannot run through agent revision {}",
                    release_target.revision_uid, revision.revision_uid
                )));
            }
            (None, _) => None,
        };
        let guardrail_policy = guardrail_policy_from_definition(
            &definition.guardrail_policy,
            model_policy.fallback_model.as_deref(),
        );
        let sandbox_policy = definition.sandbox_policy.clone();
        let instructions = instructions_from_definition(definition);
        let mut reference_paths = revision.document.reference_paths();
        if let Some(release_target_ref) = release_target_ref {
            reference_paths.push(("release_evaluation.target".to_string(), release_target_ref));
        }
        let revision_lock = match pointer.as_ref() {
            Some(pointer) if overlay.is_none() => {
                action_policy.connector_bindings = connector_bindings_from_lock(
                    &definition.action_policy,
                    &pointer.revision_lock.artifact_dependencies,
                )?;
                pointer.revision_lock.clone()
            }
            _ => {
                let resolved = self
                    .resolve_artifact_dependencies(scope, &reference_paths, overlay)
                    .await?;
                action_policy.connector_bindings = resolve_connector_bindings(
                    &definition.action_policy,
                    &resolved.connector_revisions,
                )?;
                let artifact_dependencies = resolved.artifacts;
                let tool_dependencies = locked_tools_from_definition(
                    definition,
                    &reference_paths,
                    &resolved.skill_tools,
                );
                let resolved_policy = ResolvedHashPolicy {
                    instructions: &instructions,
                    model_policy: &model_policy,
                    knowledge_policy: &knowledge_policy,
                    skill_policy: &skill_policy,
                    action_policy: &action_policy,
                    tool_policy: &tool_policy,
                    guardrail_policy: &guardrail_policy,
                    sandbox_policy: &sandbox_policy,
                };
                let policy_hash = policy_hash_for(
                    revision.revision_uid,
                    &artifact_dependencies,
                    &tool_dependencies,
                    &resolved_policy,
                )?;
                AgentRevisionLock {
                    agent_revision_uid: revision.revision_uid,
                    artifact_dependencies,
                    tool_dependencies,
                    canonical_policy_hash: policy_hash,
                }
            }
        };
        let resolved_policy = ResolvedHashPolicy {
            instructions: &instructions,
            model_policy: &model_policy,
            knowledge_policy: &knowledge_policy,
            skill_policy: &skill_policy,
            action_policy: &action_policy,
            tool_policy: &tool_policy,
            guardrail_policy: &guardrail_policy,
            sandbox_policy: &sandbox_policy,
        };
        validate_revision_lock(&revision_lock, revision.revision_uid, &resolved_policy)?;
        let policy_hash = revision_lock.canonical_policy_hash.clone();
        let artifact_dependencies = revision_lock.artifact_dependencies.clone();
        let tool_dependencies = revision_lock.tool_dependencies.clone();
        let snapshot = AgentPolicySnapshot {
            instructions: instructions.clone(),
            model_policy: model_policy.clone(),
            knowledge_policy: knowledge_policy.clone(),
            skill_policy: skill_policy.clone(),
            action_policy: action_policy.clone(),
            tool_policy: tool_policy.clone(),
            guardrail_policy: guardrail_policy.clone(),
            sandbox_policy: sandbox_policy.clone(),
            revision_lock: Some(revision_lock.clone()),
        };
        let policy_snapshot = serde_json::to_value(&snapshot)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let (agent_id, installation_uid, deployment_uid, definition_ref, display_name) =
            match pointer {
                Some(pointer) => (
                    pointer.agent_id,
                    Some(pointer.installation_uid),
                    Some(pointer.deployment_uid),
                    pointer.definition_ref,
                    pointer.display_name,
                ),
                None => (
                    None,
                    None,
                    None,
                    format!("agent://{}", revision.name),
                    definition.display_name.clone(),
                ),
            };
        let agent_context = AgentContext {
            agent_id,
            installation_uid,
            deployment_uid,
            definition_ref,
            revision_uid: revision.revision_uid,
            policy_hash,
            display_name,
            artifact_dependencies,
            tool_dependencies,
            policy_snapshot,
        };
        Ok(AgentRuntimePolicy {
            agent_context,
            revision_lock,
            instructions,
            model_policy,
            knowledge_policy,
            skill_policy,
            action_policy,
            tool_policy,
            guardrail_policy,
        })
    }

    async fn load_installation_pointer(
        &self,
        scope: &ActionRuleScope,
        installation_uid: Uuid,
    ) -> Result<Option<AgentInstallationPointer>> {
        let mut conn = scoped_conn_for_artifact_scope(&self.pool, scope).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            SELECT i.installation_uid, i.agent_id, i.artifact_uid, i.definition_ref,
                   i.display_name, i.current_revision_uid, i.last_deployment_uid,
                   d.dependency_lock, d.dependency_lock_hash
            FROM moa.agent_installation i
            JOIN moa.agent_deployment d
              ON d.deployment_uid = i.last_deployment_uid
             AND d.installation_uid = i.installation_uid
             AND d.revision_uid = i.current_revision_uid
             AND d.status = 'active'
            WHERE i.installation_uid = $3
              AND i.status = 'active'
              AND i.current_revision_uid IS NOT NULL
              AND i.storage_partition_id = $1
              AND (i.user_id IS NULL OR i.user_id = $2)
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(installation_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(pointer_from_row).transpose()
    }

    /// Resolves this agent's artifact dependencies and the tools those
    /// artifacts declare.
    ///
    /// Skill-declared tools are collected here rather than by a second pass
    /// because this is the only place the pinned skill revisions are loaded.
    /// They belong in the lock for the same reason the agent's own tools do: a
    /// skill that cannot reach the tool it was written against is a skill that
    /// silently does nothing, and a consumer reducing a loadout to fit a schema
    /// cap has no other way to know the tool was required.
    async fn resolve_artifact_dependencies(
        &self,
        scope: &ActionRuleScope,
        refs: &[(String, ArtifactRef)],
        overlay: Option<&EvalOverlayBinding>,
    ) -> Result<ResolvedAgentDependencies> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let mut seen_refs = BTreeSet::new();
        let mut loaded_revisions = BTreeMap::new();
        let mut dependencies = Vec::new();
        let mut skill_tools = Vec::new();
        for (_, artifact_ref) in refs {
            if !seen_refs.insert(artifact_ref.to_string()) {
                continue;
            }
            match artifact_ref {
                ArtifactRef::Artifact { kind, name } if *kind != ArtifactKind::Agent => {
                    let revision = load_dependency_revision(
                        &registry,
                        scope,
                        &mut loaded_revisions,
                        kind.clone(),
                        name,
                        artifact_ref,
                        overlay,
                    )
                    .await?;
                    skill_tools.extend(skill_declared_tools(&revision));
                    dependencies.push(resolved_dependency(artifact_ref, &revision));
                }
                ArtifactRef::Action { connector, .. } => {
                    let revision = load_dependency_revision(
                        &registry,
                        scope,
                        &mut loaded_revisions,
                        ArtifactKind::Connector,
                        connector,
                        artifact_ref,
                        overlay,
                    )
                    .await?;
                    dependencies.push(resolved_dependency(artifact_ref, &revision));
                }
                ArtifactRef::Artifact { .. } | ArtifactRef::Tool { .. } => {}
            }
        }
        dependencies.sort_by(|left, right| left.reference.cmp(&right.reference));
        dependencies.dedup_by(|left, right| left.reference == right.reference);
        skill_tools.sort();
        skill_tools.dedup();
        let connector_revisions = loaded_revisions
            .into_values()
            .filter(|revision| revision.kind == ArtifactKind::Connector)
            .filter_map(|revision| {
                let ArtifactDefinition::Connector(definition) = revision.document.definition else {
                    return None;
                };
                Some((
                    revision.name,
                    ResolvedConnectorRevision {
                        artifact_uid: revision.artifact_uid,
                        revision_uid: revision.revision_uid,
                        definition,
                    },
                ))
            })
            .collect();
        Ok(ResolvedAgentDependencies {
            artifacts: dependencies,
            skill_tools,
            connector_revisions,
        })
    }
}

/// One agent's resolved artifact dependencies and the tools they declare.
struct ResolvedAgentDependencies {
    artifacts: Vec<ResolvedArtifactRevisionRef>,
    skill_tools: Vec<String>,
    connector_revisions: BTreeMap<String, ResolvedConnectorRevision>,
}

#[derive(Clone)]
struct ResolvedConnectorRevision {
    artifact_uid: Uuid,
    revision_uid: Uuid,
    definition: moa_artifacts::connector::ConnectorDefinition,
}

/// Returns the registered tool names one activated skill revision declares.
///
/// Both declaration sites count: `allowed_tools` is the skill's stated tool
/// surface, and a `Tool`-kind action's reference is a tool the skill will
/// actually invoke. A skill that lists a tool in neither place is not depending
/// on it.
fn skill_declared_tools(revision: &StoredArtifactRevision) -> Vec<String> {
    let ArtifactDefinition::Skill(skill) = &revision.document.definition else {
        return Vec::new();
    };
    skill
        .allowed_tools
        .iter()
        .cloned()
        .chain(
            skill
                .actions
                .iter()
                .filter_map(|action| match action.artifact_ref.as_ref() {
                    Some(ArtifactRef::Tool { name }) => Some(name.clone()),
                    _ => None,
                }),
        )
        .collect()
}

async fn load_dependency_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    loaded_revisions: &mut BTreeMap<(String, String), StoredArtifactRevision>,
    kind: ArtifactKind,
    name: &str,
    artifact_ref: &ArtifactRef,
    overlay: Option<&EvalOverlayBinding>,
) -> Result<StoredArtifactRevision> {
    let key = (kind.as_str().to_string(), name.to_string());
    if let Some(revision) = loaded_revisions.get(&key) {
        return Ok(revision.clone());
    }

    // Release-gated artifacts resolve through their type-owned serving pointer.
    // Other kinds retain their established published-revision lifecycle.
    let revision = match kind {
        ArtifactKind::Skill | ArtifactKind::Action => {
            registry
                .load_serving_with_overlay(scope, kind, name, overlay)
                .await?
        }
        _ => registry.load_visible_published(scope, kind, name).await?,
    }
    .ok_or_else(|| unresolved_dependency(artifact_ref))?;
    loaded_revisions.insert(key, revision.clone());
    Ok(revision)
}

async fn load_agent_revision(
    pool: &PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let registry = ArtifactRegistry::new(pool.clone());
    let revision = registry
        .load_revision(scope, revision_uid)
        .await?
        .ok_or_else(|| {
            MoaError::StorageError(format!(
                "agent revision {revision_uid} not found or not visible"
            ))
        })?;
    if revision.kind != ArtifactKind::Agent {
        return Err(MoaError::ValidationError(format!(
            "artifact revision {} is {}, expected agent",
            revision.revision_uid, revision.kind
        )));
    }
    Ok(revision)
}

async fn load_executable_agent_revision(
    pool: &PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let revision = load_agent_revision(pool, scope, revision_uid).await?;
    if !is_exact_agent_revision_executable(&revision, false)? {
        return Err(MoaError::ValidationError(format!(
            "agent revision {} is not executable in state {}",
            revision.revision_uid, revision.status
        )));
    }
    Ok(revision)
}

async fn load_agent_revision_for_evaluation(
    pool: &PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let revision = load_agent_revision(pool, scope, revision_uid).await?;
    if !is_exact_agent_revision_executable(&revision, true)? {
        return Err(MoaError::ValidationError(format!(
            "agent revision {} is not executable in evaluation state {}",
            revision.revision_uid, revision.status
        )));
    }
    Ok(revision)
}

fn is_exact_agent_revision_executable(
    revision: &StoredArtifactRevision,
    allow_evaluating: bool,
) -> Result<bool> {
    let state = ReleaseState::from_artifact_status(&revision.status)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    Ok(
        matches!(state, ReleaseState::Ready | ReleaseState::Superseded)
            || allow_evaluating && state == ReleaseState::Evaluating,
    )
}

async fn load_agent_release_candidate(
    pool: &PgPool,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let revision = load_agent_revision(pool, scope, revision_uid).await?;
    let state = ReleaseState::from_artifact_status(&revision.status)
        .map_err(|error| MoaError::ValidationError(error.to_string()))?;
    if matches!(state, ReleaseState::Rejected | ReleaseState::Archived) {
        return Err(MoaError::ValidationError(format!(
            "agent revision {} cannot build a release lock from state {}",
            revision.revision_uid, revision.status
        )));
    }
    Ok(revision)
}

async fn scoped_conn_for_artifact_scope<'p>(
    pool: &'p PgPool,
    scope: &ActionRuleScope,
) -> Result<ScopedConn<'p>> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => ScopedConn::begin_tenant(pool, *tenant_id).await,
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => ScopedConn::begin_contact(pool, *tenant_id, *contact_id).await,
    }
}

fn agent_definition(revision: &StoredArtifactRevision) -> Result<&AgentDefinition> {
    match &revision.document.definition {
        ArtifactDefinition::Agent(definition) => Ok(definition.as_ref()),
        _ => Err(MoaError::ValidationError(format!(
            "agent revision {} has a non-agent definition",
            revision.revision_uid
        ))),
    }
}

fn instructions_from_definition(definition: &AgentDefinition) -> Vec<String> {
    let mut instructions = Vec::new();
    if !definition
        .instruction_policy
        .system_prompt
        .trim()
        .is_empty()
    {
        instructions.push(definition.instruction_policy.system_prompt.clone());
    }
    instructions.extend(
        definition
            .instruction_policy
            .instructions
            .iter()
            .filter(|instruction| !instruction.trim().is_empty())
            .cloned(),
    );
    if let Some(default_task) = definition
        .purpose
        .default_task
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        instructions.push(format!("Default task: {default_task}"));
    }
    instructions
}

fn model_policy_from_definition(definition: &ModelPolicy) -> AgentModelPolicy {
    AgentModelPolicy {
        default_model: definition.default_model.clone(),
        allowed_models: sorted_unique(&definition.allowed_models),
        fallback_model: definition.fallback_model.clone(),
    }
}

fn knowledge_policy_from_definition(definition: &AgentDefinition) -> AgentKnowledgePolicy {
    AgentKnowledgePolicy {
        mode: match definition.knowledge_policy.mode {
            KnowledgeScopeMode::Enabled => AgentKnowledgeScopeMode::Enabled,
            KnowledgeScopeMode::Disabled => AgentKnowledgeScopeMode::Disabled,
        },
        filters: definition.knowledge_policy.filters.clone(),
        retrieval_budget: definition.knowledge_policy.retrieval_budget,
        pii_floor: definition.knowledge_policy.pii_floor.clone(),
        // Operator-authored clearances flow from the definition to the runtime
        // policy. The definition field defaults to empty, so an agent with no
        // authored clearances stays fail-closed (sees no barriered data).
        cleared_barriers: definition.knowledge_policy.cleared_barriers.clone(),
        write_barrier: definition.knowledge_policy.write_barrier.clone(),
    }
}

fn skill_policy_from_definition(definition: &SkillPolicy) -> AgentSkillPolicy {
    AgentSkillPolicy {
        mode: match definition.mode {
            SkillPolicyMode::Auto => AgentSkillPolicyMode::Auto,
            SkillPolicyMode::Allowlist => AgentSkillPolicyMode::Allowlist,
            SkillPolicyMode::Pinned => AgentSkillPolicyMode::Pinned,
            SkillPolicyMode::Denylist => AgentSkillPolicyMode::Denylist,
        },
        refs: sorted_unique(&definition.refs),
        max_visible: definition.max_visible,
    }
}

fn action_policy_from_definition(definition: &ActionPolicy) -> AgentActionPolicy {
    AgentActionPolicy {
        allowed: sorted_unique(&definition.allowed),
        require_admin_review: sorted_unique(&definition.require_admin_review),
        connector_bindings: Vec::new(),
    }
}

fn resolve_connector_bindings(
    definition: &ActionPolicy,
    connector_revisions: &BTreeMap<String, ResolvedConnectorRevision>,
) -> Result<Vec<AgentConnectorBinding>> {
    let mut bindings = Vec::with_capacity(definition.connector_bindings.len());
    for binding in &definition.connector_bindings {
        let connector_name = connector_binding_name(&binding.connector_ref)?;
        let connector_ref = binding
            .connector_ref
            .canonical_string()
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;
        let revision = connector_revisions.get(connector_name).ok_or_else(|| {
            MoaError::ValidationError(format!(
                "agent connector binding `{connector_ref}` did not resolve a published revision"
            ))
        })?;
        if !revision.definition.is_connection_installable() {
            return Err(MoaError::ValidationError(format!(
                "agent connector binding `{connector_ref}` targets a legacy connector definition"
            )));
        }
        bindings.push(AgentConnectorBinding {
            connector_ref,
            connection_id: binding.connection_id,
            artifact_uid: revision.artifact_uid,
            revision_uid: revision.revision_uid,
        });
    }
    validate_connector_binding_uniqueness(&bindings)?;
    canonicalize_connector_bindings(&mut bindings);

    for artifact_ref in definition
        .allowed
        .iter()
        .chain(&definition.require_admin_review)
    {
        let ArtifactRef::Action { connector, .. } = artifact_ref else {
            continue;
        };
        let Some(revision) = connector_revisions.get(connector) else {
            return Err(unresolved_dependency(artifact_ref));
        };
        if revision.definition.runtime_v1().is_some() {
            let required_ref = ArtifactRef::connector(connector.clone()).to_string();
            let count = bindings
                .iter()
                .filter(|binding| binding.connector_ref == required_ref)
                .count();
            if count != 1 {
                return Err(MoaError::ValidationError(format!(
                    "runtime connector action `{artifact_ref}` requires exactly one binding for `{required_ref}`"
                )));
            }
        }
    }
    Ok(bindings)
}

fn connector_bindings_from_lock(
    definition: &ActionPolicy,
    artifact_dependencies: &[ResolvedArtifactRevisionRef],
) -> Result<Vec<AgentConnectorBinding>> {
    let mut bindings = Vec::with_capacity(definition.connector_bindings.len());
    for binding in &definition.connector_bindings {
        let connector_ref = binding
            .connector_ref
            .canonical_string()
            .map_err(|error| MoaError::ValidationError(error.to_string()))?;
        connector_binding_name(&binding.connector_ref)?;
        let dependency = artifact_dependencies
            .iter()
            .find(|dependency| dependency.reference == connector_ref)
            .ok_or_else(|| {
                MoaError::ValidationError(format!(
                    "agent revision lock is missing connector binding dependency `{connector_ref}`"
                ))
            })?;
        if dependency.kind != ArtifactKind::Connector.as_str() {
            return Err(MoaError::ValidationError(format!(
                "agent revision lock dependency `{connector_ref}` is not a connector"
            )));
        }
        bindings.push(AgentConnectorBinding {
            connector_ref,
            connection_id: binding.connection_id,
            artifact_uid: dependency.artifact_uid,
            revision_uid: dependency.revision_uid,
        });
    }
    validate_connector_binding_uniqueness(&bindings)?;
    canonicalize_connector_bindings(&mut bindings);
    Ok(bindings)
}

fn connector_binding_name(artifact_ref: &ArtifactRef) -> Result<&str> {
    match artifact_ref {
        ArtifactRef::Artifact {
            kind: ArtifactKind::Connector,
            name,
        } => Ok(name),
        _ => Err(MoaError::ValidationError(format!(
            "agent connector binding `{artifact_ref}` must use connector://"
        ))),
    }
}

fn canonicalize_connector_bindings(bindings: &mut [AgentConnectorBinding]) {
    bindings.sort_by(|left, right| {
        left.connector_ref
            .cmp(&right.connector_ref)
            .then_with(|| left.connection_id.0.cmp(&right.connection_id.0))
            .then_with(|| left.artifact_uid.cmp(&right.artifact_uid))
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
}

fn validate_connector_binding_uniqueness(bindings: &[AgentConnectorBinding]) -> Result<()> {
    let mut connector_refs = BTreeSet::new();
    let mut connection_ids = BTreeSet::new();
    for binding in bindings {
        if !connector_refs.insert(binding.connector_ref.as_str()) {
            return Err(MoaError::ValidationError(format!(
                "agent action policy binds connector `{}` more than once",
                binding.connector_ref
            )));
        }
        if !connection_ids.insert(binding.connection_id.0) {
            return Err(MoaError::ValidationError(format!(
                "agent action policy binds connection {} more than once",
                binding.connection_id
            )));
        }
    }
    Ok(())
}

fn tool_policy_from_definition(definition: &AgentDefinition) -> AgentToolPolicy {
    AgentToolPolicy {
        mode: match definition.tool_policy.mode {
            ToolPolicyMode::Auto => AgentToolPolicyMode::Auto,
            ToolPolicyMode::Allowlist => AgentToolPolicyMode::Allowlist,
            ToolPolicyMode::Denylist => AgentToolPolicyMode::Denylist,
        },
        tools: sorted_unique(&definition.tool_policy.tools),
        denied_tools: sorted_unique(&definition.tool_policy.denied_tools),
    }
}

fn guardrail_policy_from_definition(
    definition: &GuardrailPolicy,
    fallback_model: Option<&str>,
) -> AgentGuardrailPolicy {
    AgentGuardrailPolicy {
        input: definition
            .input
            .as_ref()
            .map(|stage| guardrail_stage_policy_from_definition(stage, fallback_model)),
        output: definition
            .output
            .as_ref()
            .map(|stage| guardrail_stage_policy_from_definition(stage, fallback_model)),
    }
}

fn guardrail_stage_policy_from_definition(
    definition: &GuardrailStagePolicy,
    fallback_model: Option<&str>,
) -> AgentGuardrailStagePolicy {
    let effective_model = definition
        .model
        .as_deref()
        .or_else(|| definition.enabled.then_some(fallback_model).flatten());
    AgentGuardrailStagePolicy {
        enabled: definition.enabled,
        mode: definition.mode,
        model: effective_model.map(ModelId::new),
        policy_prompt: definition.policy_prompt.clone(),
        block_message: definition.block_message.clone(),
    }
}

fn sorted_unique<T: ToString>(values: &[T]) -> Vec<String> {
    let mut values = values.iter().map(ToString::to_string).collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

/// Builds the exact tool set this agent revision locks.
///
/// `skill_tools` are the tools the agent's pinned skills declare. They are
/// locked alongside the agent's own tools but stay subject to the agent's
/// `denied_tools`: a skill cannot grant itself a tool the agent explicitly
/// refused, or a skill dependency would become a way around the agent's policy.
fn locked_tools_from_definition(
    definition: &AgentDefinition,
    refs: &[(String, ArtifactRef)],
    skill_tools: &[String],
) -> Vec<LockedToolRef> {
    let denied_tools = definition
        .tool_policy
        .denied_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut tools = definition
        .tool_policy
        .tools
        .iter()
        .cloned()
        .chain(
            refs.iter()
                .filter_map(|(path, artifact_ref)| match artifact_ref {
                    ArtifactRef::Tool { name }
                        if !path.contains(".denied_tools") && !denied_tools.contains(name) =>
                    {
                        Some(name.clone())
                    }
                    ArtifactRef::Artifact { .. } | ArtifactRef::Action { .. } => None,
                    ArtifactRef::Tool { .. } => None,
                }),
        )
        .chain(
            skill_tools
                .iter()
                .filter(|name| !denied_tools.contains(*name))
                .cloned(),
        )
        .map(|name| LockedToolRef {
            identity_hash: stable_tool_identity_hash(&name, "builtin"),
            name,
            provider: Some("builtin".to_string()),
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools.dedup_by(|left, right| left.name == right.name);
    tools
}

/// Hashes one locked dependency's identity: its name and provider namespace.
///
/// Deliberately not a schema hash — see [`LockedToolRef::identity_hash`], which
/// documents what this does and does not pin. The name says so, so a reader
/// cannot mistake it for a contract check.
fn stable_tool_identity_hash(name: &str, provider: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"moa.tool-lock.v2");
    hasher.update((provider.len() as u64).to_be_bytes());
    hasher.update(provider.as_bytes());
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name.as_bytes());
    hex::encode(hasher.finalize())
}

fn policy_hash_for(
    agent_revision_uid: Uuid,
    artifact_dependencies: &[ResolvedArtifactRevisionRef],
    tool_dependencies: &[LockedToolRef],
    policy: &ResolvedHashPolicy<'_>,
) -> Result<String> {
    #[derive(Serialize)]
    struct HashInput<'a> {
        agent_revision_uid: Uuid,
        artifact_dependencies: &'a [ResolvedArtifactRevisionRef],
        tool_dependencies: &'a [LockedToolRef],
        instructions: &'a [String],
        model_policy: &'a AgentModelPolicy,
        knowledge_policy: &'a AgentKnowledgePolicy,
        skill_policy: &'a AgentSkillPolicy,
        action_policy: &'a AgentActionPolicy,
        tool_policy: &'a AgentToolPolicy,
        guardrail_policy: &'a AgentGuardrailPolicy,
        sandbox_policy: &'a AgentSandboxPolicy,
    }

    let digest = canonical_hash(&HashInput {
        agent_revision_uid,
        artifact_dependencies,
        tool_dependencies,
        instructions: policy.instructions,
        model_policy: policy.model_policy,
        knowledge_policy: policy.knowledge_policy,
        skill_policy: policy.skill_policy,
        action_policy: policy.action_policy,
        tool_policy: policy.tool_policy,
        guardrail_policy: policy.guardrail_policy,
        sandbox_policy: policy.sandbox_policy,
    })
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    Ok(hex::encode(digest))
}

fn unresolved_dependency(artifact_ref: &ArtifactRef) -> MoaError {
    MoaError::ValidationError(format!(
        "agent dependency {artifact_ref} is not visible as a published artifact"
    ))
}

fn resolved_dependency(
    artifact_ref: &ArtifactRef,
    revision: &StoredArtifactRevision,
) -> ResolvedArtifactRevisionRef {
    ResolvedArtifactRevisionRef {
        reference: artifact_ref.to_string(),
        kind: revision.kind.to_string(),
        name: revision.name.clone(),
        artifact_uid: revision.artifact_uid,
        revision_uid: revision.revision_uid,
        version: revision.version,
    }
}

struct ResolvedHashPolicy<'a> {
    instructions: &'a [String],
    model_policy: &'a AgentModelPolicy,
    knowledge_policy: &'a AgentKnowledgePolicy,
    skill_policy: &'a AgentSkillPolicy,
    action_policy: &'a AgentActionPolicy,
    tool_policy: &'a AgentToolPolicy,
    guardrail_policy: &'a AgentGuardrailPolicy,
    sandbox_policy: &'a AgentSandboxPolicy,
}

fn validate_revision_lock(
    revision_lock: &AgentRevisionLock,
    revision_uid: Uuid,
    policy: &ResolvedHashPolicy<'_>,
) -> Result<()> {
    if revision_lock.agent_revision_uid != revision_uid {
        return Err(MoaError::ValidationError(format!(
            "agent deployment lock points at revision {}, expected {}",
            revision_lock.agent_revision_uid, revision_uid
        )));
    }
    let expected_hash = policy_hash_for(
        revision_uid,
        &revision_lock.artifact_dependencies,
        &revision_lock.tool_dependencies,
        policy,
    )?;
    if revision_lock.canonical_policy_hash != expected_hash {
        return Err(MoaError::ValidationError(format!(
            "agent deployment lock hash mismatch for revision {}",
            revision_uid
        )));
    }
    Ok(())
}

fn pointer_from_row(row: &sqlx::postgres::PgRow) -> Result<AgentInstallationPointer> {
    let current_revision_uid = row
        .try_get::<Option<Uuid>, _>("current_revision_uid")
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError("active agent installation missing current revision".to_string())
        })?;
    let Json(revision_lock) = row
        .try_get::<Json<AgentRevisionLock>, _>("dependency_lock")
        .map_err(map_sqlx_error)?;
    let deployment_uid = row
        .try_get::<Option<Uuid>, _>("last_deployment_uid")
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError("active agent installation missing deployment".to_string())
        })?;
    let dependency_lock_hash: String = row
        .try_get("dependency_lock_hash")
        .map_err(map_sqlx_error)?;
    if dependency_lock_hash != revision_lock.canonical_policy_hash {
        return Err(MoaError::ValidationError(format!(
            "agent deployment {deployment_uid} lock hash does not match stored dependency lock"
        )));
    }
    Ok(AgentInstallationPointer {
        installation_uid: row.try_get("installation_uid").map_err(map_sqlx_error)?,
        agent_id: row.try_get("agent_id").map_err(map_sqlx_error)?,
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        definition_ref: row.try_get("definition_ref").map_err(map_sqlx_error)?,
        display_name: row.try_get("display_name").map_err(map_sqlx_error)?,
        current_revision_uid,
        deployment_uid,
        revision_lock,
    })
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use moa_artifacts::agent::{AgentPurpose, ConnectorBinding, KnowledgePolicy, ToolPolicy};
    use moa_core::types::guardrails::GuardrailMode;
    use moa_core::types::identifiers::ConnectorConnectionId;

    use super::*;

    #[test]
    fn authored_clearance_reaches_runtime_knowledge_policy() {
        // Pins: an operator-authored barrier clearance on the definition is copied
        // onto the runtime knowledge policy the retrieval stage consumes.
        let definition = AgentDefinition {
            knowledge_policy: KnowledgePolicy {
                cleared_barriers: [moa_core::types::memory::InformationBarrierId::parse(
                    "deal-alpha",
                )
                .expect("valid barrier")]
                .into_iter()
                .collect(),
                write_barrier: Some(
                    moa_core::types::memory::InformationBarrierId::parse("deal-alpha")
                        .expect("valid barrier"),
                ),
                ..KnowledgePolicy::default()
            },
            ..agent_definition()
        };

        let policy = knowledge_policy_from_definition(&definition);

        assert_eq!(
            policy
                .cleared_barriers
                .iter()
                .map(moa_core::types::memory::InformationBarrierId::as_str)
                .collect::<Vec<_>>(),
            vec!["deal-alpha"]
        );
        assert_eq!(
            policy
                .write_barrier
                .as_ref()
                .map(|barrier| barrier.as_str()),
            Some("deal-alpha")
        );
    }

    #[test]
    fn absent_clearance_fails_closed_with_empty_barriers() {
        // Pins: a definition without authored clearances resolves to an empty
        // clearance set so barriered data stays hidden (fail-closed default).
        let definition = agent_definition();

        let policy = knowledge_policy_from_definition(&definition);

        assert!(policy.cleared_barriers.is_empty());
    }

    #[test]
    fn enabled_guardrail_stage_snapshots_effective_fallback_model() {
        // Pins: enabled guardrail model fallback is resolved into the pinned policy hash input.
        let definition = AgentDefinition {
            model_policy: ModelPolicy {
                fallback_model: Some("anthropic:claude-haiku-4-5".to_string()),
                ..ModelPolicy::default()
            },
            guardrail_policy: GuardrailPolicy {
                input: Some(GuardrailStagePolicy {
                    enabled: true,
                    mode: GuardrailMode::Shadow,
                    model: None,
                    policy_prompt: "Flag unsafe requests.".to_string(),
                    block_message: None,
                }),
                output: None,
            },
            ..agent_definition()
        };

        let policy = guardrail_policy_from_definition(
            &definition.guardrail_policy,
            definition.model_policy.fallback_model.as_deref(),
        );

        assert_eq!(
            policy
                .input
                .as_ref()
                .and_then(|stage| stage.model.as_ref())
                .map(ModelId::as_str),
            Some("anthropic:claude-haiku-4-5")
        );
    }

    #[test]
    fn locked_tools_excludes_denied_tools_and_records_provider_identity() {
        // Pins: denied tools do not inflate the dependency lock for effective allowed tools.
        let definition = AgentDefinition {
            tool_policy: ToolPolicy {
                mode: ToolPolicyMode::Allowlist,
                tools: vec!["file_read".to_string()],
                denied_tools: vec!["shell".to_string()],
            },
            ..agent_definition()
        };
        let refs = definition.reference_paths();

        let locked = locked_tools_from_definition(&definition, &refs, &[]);

        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].name, "file_read");
        assert_eq!(locked[0].provider.as_deref(), Some("builtin"));
        assert_eq!(
            locked[0].identity_hash,
            stable_tool_identity_hash("file_read", "builtin")
        );
    }

    #[test]
    fn locked_tools_include_skill_declared_tools_but_never_a_denied_one() {
        // Pins: a pinned skill's declared tools enter the agent's dependency
        // lock, so a loadout forced to fit a schema cap keeps them; and a skill
        // cannot use that path to reintroduce a tool the agent denied, which
        // would make a skill dependency a way around agent policy.
        let definition = AgentDefinition {
            tool_policy: ToolPolicy {
                mode: ToolPolicyMode::Allowlist,
                tools: vec!["file_read".to_string()],
                denied_tools: vec!["shell".to_string()],
            },
            ..agent_definition()
        };
        let refs = definition.reference_paths();

        let locked = locked_tools_from_definition(
            &definition,
            &refs,
            &["mcp__crm__lookup".to_string(), "shell".to_string()],
        );

        let names = locked
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["file_read", "mcp__crm__lookup"]);
    }

    #[test]
    fn connector_binding_empty_policy_preserves_pre_t1_hash_and_revision_lock() {
        // Pins: adding serde-omitted connector bindings must not invalidate a
        // previously persisted no-binding policy hash or deployment lock.
        let revision_uid = Uuid::from_u128(0xfeed);
        let action_policy = AgentActionPolicy {
            allowed: vec!["action://refund".to_string()],
            require_admin_review: vec!["action://refund".to_string()],
            connector_bindings: Vec::new(),
        };
        let fixed_pre_t1_hash = "218a327081d3d33b77bf1f23cc934b5ebbda61a296fde601651bfa0fefad93ca";

        assert_eq!(
            test_policy_hash(revision_uid, &action_policy),
            fixed_pre_t1_hash
        );

        let lock = AgentRevisionLock {
            agent_revision_uid: revision_uid,
            artifact_dependencies: Vec::new(),
            tool_dependencies: Vec::new(),
            canonical_policy_hash: fixed_pre_t1_hash.to_string(),
        };
        let instructions = Vec::new();
        let model_policy = AgentModelPolicy::default();
        let knowledge_policy = AgentKnowledgePolicy::default();
        let skill_policy = AgentSkillPolicy::default();
        let tool_policy = AgentToolPolicy::default();
        let guardrail_policy = AgentGuardrailPolicy::default();
        let sandbox_policy = AgentSandboxPolicy::default();
        let policy = ResolvedHashPolicy {
            instructions: &instructions,
            model_policy: &model_policy,
            knowledge_policy: &knowledge_policy,
            skill_policy: &skill_policy,
            action_policy: &action_policy,
            tool_policy: &tool_policy,
            guardrail_policy: &guardrail_policy,
            sandbox_policy: &sandbox_policy,
        };

        validate_revision_lock(&lock, revision_uid, &policy)
            .expect("the pre-T1 no-binding revision lock should remain valid");
    }

    #[test]
    fn connector_binding_order_is_hash_stable_and_connection_is_significant() {
        // Pins: authoring order cannot churn the revision lock, while selecting
        // another installed connection must change the replay-stable policy hash.
        let first = AgentConnectorBinding {
            connector_ref: "connector://billing".to_string(),
            connection_id: ConnectorConnectionId(Uuid::from_u128(1)),
            artifact_uid: Uuid::from_u128(10),
            revision_uid: Uuid::from_u128(11),
        };
        let second = AgentConnectorBinding {
            connector_ref: "connector://crm".to_string(),
            connection_id: ConnectorConnectionId(Uuid::from_u128(2)),
            artifact_uid: Uuid::from_u128(20),
            revision_uid: Uuid::from_u128(21),
        };
        let mut ordered = vec![first.clone(), second.clone()];
        let mut reversed = vec![second, first.clone()];
        canonicalize_connector_bindings(&mut ordered);
        canonicalize_connector_bindings(&mut reversed);
        let ordered_policy = AgentActionPolicy {
            connector_bindings: ordered,
            ..AgentActionPolicy::default()
        };
        let reversed_policy = AgentActionPolicy {
            connector_bindings: reversed,
            ..AgentActionPolicy::default()
        };

        let ordered_hash = test_policy_hash(Uuid::from_u128(99), &ordered_policy);
        assert_eq!(
            ordered_hash,
            test_policy_hash(Uuid::from_u128(99), &reversed_policy)
        );

        let mut changed_policy = ordered_policy;
        changed_policy.connector_bindings[0].connection_id =
            ConnectorConnectionId(Uuid::from_u128(3));
        canonicalize_connector_bindings(&mut changed_policy.connector_bindings);
        assert_ne!(
            ordered_hash,
            test_policy_hash(Uuid::from_u128(99), &changed_policy)
        );
    }

    #[test]
    fn connector_binding_resolution_pins_runtime_revision_and_requires_coverage() {
        // Pins: each referenced runtime connector resolves to one exact
        // connection/artifact/revision tuple; legacy aliases need no binding.
        let runtime = resolved_connector_test_revision(Uuid::from_u128(31), true);
        let legacy = resolved_connector_test_revision(Uuid::from_u128(41), false);
        let revisions = BTreeMap::from([
            ("billing".to_string(), runtime.clone()),
            ("legacy-crm".to_string(), legacy),
        ]);
        let connection_id = ConnectorConnectionId(Uuid::from_u128(51));
        let definition = ActionPolicy {
            allowed: vec![ArtifactRef::action("billing", "charge")],
            connector_bindings: vec![ConnectorBinding {
                connector_ref: ArtifactRef::connector("billing"),
                connection_id,
            }],
            ..ActionPolicy::default()
        };

        assert_eq!(
            resolve_connector_bindings(&definition, &revisions)
                .expect("runtime connector binding should resolve"),
            vec![AgentConnectorBinding {
                connector_ref: "connector://billing".to_string(),
                connection_id,
                artifact_uid: runtime.artifact_uid,
                revision_uid: runtime.revision_uid,
            }]
        );

        let missing = ActionPolicy {
            connector_bindings: Vec::new(),
            ..definition
        };
        let error = resolve_connector_bindings(&missing, &revisions)
            .expect_err("a referenced runtime connector must be bound");
        assert!(matches!(error, MoaError::ValidationError(message)
            if message.contains("requires exactly one binding")));

        let legacy_only = ActionPolicy {
            allowed: vec![ArtifactRef::action("legacy-crm", "lookup")],
            ..ActionPolicy::default()
        };
        assert_eq!(
            resolve_connector_bindings(&legacy_only, &revisions)
                .expect("legacy connector aliases remain binding-free"),
            Vec::new()
        );
    }

    fn test_policy_hash(revision_uid: Uuid, action_policy: &AgentActionPolicy) -> String {
        let instructions = Vec::new();
        let model_policy = AgentModelPolicy::default();
        let knowledge_policy = AgentKnowledgePolicy::default();
        let skill_policy = AgentSkillPolicy::default();
        let tool_policy = AgentToolPolicy::default();
        let guardrail_policy = AgentGuardrailPolicy::default();
        let sandbox_policy = AgentSandboxPolicy::default();
        policy_hash_for(
            revision_uid,
            &[],
            &[],
            &ResolvedHashPolicy {
                instructions: &instructions,
                model_policy: &model_policy,
                knowledge_policy: &knowledge_policy,
                skill_policy: &skill_policy,
                action_policy,
                tool_policy: &tool_policy,
                guardrail_policy: &guardrail_policy,
                sandbox_policy: &sandbox_policy,
            },
        )
        .expect("test policy should hash")
    }

    fn resolved_connector_test_revision(
        revision_uid: Uuid,
        runtime: bool,
    ) -> ResolvedConnectorRevision {
        let spec = if runtime {
            serde_json::json!({
                "definition_version": "v1",
                "display_name": "Billing",
                "runtime": {"type": "mcp"},
                "auth": [{"type": "none"}],
                "actions": [{
                    "id": "charge",
                    "binding": {
                        "type": "mcp",
                        "remote_operation": "charge",
                        "contract": {
                            "input_schema": {"type": "object"},
                            "output_schema": {"type": "object"},
                            "data_classes": ["none"],
                            "action_class": "external_write",
                            "risk_level": "high",
                            "minimum_effect": "admin_review",
                            "idempotency": "non_idempotent"
                        }
                    }
                }]
            })
        } else {
            serde_json::json!({
                "auth": {},
                "actions": [{
                    "id": "lookup",
                    "description": "legacy lookup",
                    "tool_name": "file_read",
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "admin_review_required": false,
                    "ui": {}
                }],
                "ui": {}
            })
        };
        ResolvedConnectorRevision {
            artifact_uid: Uuid::from_u128(revision_uid.as_u128() + 1_000),
            revision_uid,
            definition: serde_json::from_value(spec)
                .expect("connector definition fixture should deserialize"),
        }
    }

    fn agent_definition() -> AgentDefinition {
        AgentDefinition {
            display_name: "Support".to_string(),
            purpose: AgentPurpose::default(),
            model_policy: ModelPolicy::default(),
            instruction_policy: Default::default(),
            knowledge_policy: Default::default(),
            skill_policy: Default::default(),
            action_policy: Default::default(),
            tool_policy: Default::default(),
            guardrail_policy: Default::default(),
            sandbox_policy: Default::default(),
            revision_note: None,
            metadata: serde_json::json!({}),
        }
    }
}
