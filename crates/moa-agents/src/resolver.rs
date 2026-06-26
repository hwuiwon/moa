//! Resolver for installed and exact configured-agent revisions.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::agent::{
    ActionPolicy, AgentDefinition, GuardrailPolicy, GuardrailStagePolicy, KnowledgeScopeMode,
    ModelPolicy, SkillPolicy, SkillPolicyMode, ToolPolicyMode, WorkflowPolicy,
};
use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, ArtifactScopeParts, StoredArtifactRevision};
use moa_core::{
    ActionRuleScope, AgentActionPolicy, AgentContext, AgentGuardrailPolicy,
    AgentGuardrailStagePolicy, AgentKnowledgePolicy, AgentKnowledgeScopeMode, AgentModelPolicy,
    AgentPolicySnapshot, AgentRevisionLock, AgentSkillPolicy, AgentSkillPolicyMode,
    AgentToolPolicy, AgentToolPolicyMode, AgentWorkflowPolicy, LockedToolRef, MoaError, ModelId,
    ResolvedArtifactRevisionRef, Result,
};
use moa_db::ScopedConn;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row, types::Json};
use uuid::Uuid;

use crate::definition::AgentInstallationPointer;
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
        self.resolve_revision_with_pointer(scope, pointer).await
    }

    /// Resolves an exact published agent revision without moving any deployment pointer.
    pub async fn resolve_exact_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<AgentRuntimePolicy> {
        let revision = load_agent_revision(&self.pool, scope, revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, None).await
    }

    async fn resolve_revision_with_pointer(
        &self,
        scope: &ActionRuleScope,
        pointer: AgentInstallationPointer,
    ) -> Result<AgentRuntimePolicy> {
        let revision = load_agent_revision(&self.pool, scope, pointer.current_revision_uid).await?;
        self.resolve_loaded_revision(scope, revision, Some(pointer))
            .await
    }

    async fn resolve_loaded_revision(
        &self,
        scope: &ActionRuleScope,
        revision: StoredArtifactRevision,
        pointer: Option<AgentInstallationPointer>,
    ) -> Result<AgentRuntimePolicy> {
        let definition = agent_definition(&revision)?;
        let tool_policy = tool_policy_from_definition(definition);
        let model_policy = model_policy_from_definition(&definition.model_policy);
        let knowledge_policy = knowledge_policy_from_definition(definition);
        let skill_policy = skill_policy_from_definition(&definition.skill_policy);
        let workflow_policy = workflow_policy_from_definition(&definition.workflow_policy);
        let action_policy = action_policy_from_definition(&definition.action_policy);
        let guardrail_policy = guardrail_policy_from_definition(
            &definition.guardrail_policy,
            model_policy.fallback_model.as_deref(),
        );
        let instructions = instructions_from_definition(definition);
        let resolved_policy = ResolvedHashPolicy {
            instructions: &instructions,
            model_policy: &model_policy,
            knowledge_policy: &knowledge_policy,
            skill_policy: &skill_policy,
            workflow_policy: &workflow_policy,
            action_policy: &action_policy,
            tool_policy: &tool_policy,
            guardrail_policy: &guardrail_policy,
        };
        let revision_lock = match pointer.as_ref() {
            Some(pointer) => pointer.revision_lock.clone(),
            None => {
                let reference_paths = revision.document.reference_paths();
                let artifact_dependencies = self
                    .resolve_artifact_dependencies(scope, &reference_paths)
                    .await?;
                let tool_dependencies = locked_tools_from_definition(definition, &reference_paths);
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
        validate_revision_lock(&revision_lock, &revision, &resolved_policy)?;
        let policy_hash = revision_lock.canonical_policy_hash.clone();
        let artifact_dependencies = revision_lock.artifact_dependencies.clone();
        let tool_dependencies = revision_lock.tool_dependencies.clone();
        let snapshot = AgentPolicySnapshot {
            instructions: instructions.clone(),
            model_policy: model_policy.clone(),
            knowledge_policy: knowledge_policy.clone(),
            skill_policy: skill_policy.clone(),
            workflow_policy: workflow_policy.clone(),
            action_policy: action_policy.clone(),
            tool_policy: tool_policy.clone(),
            guardrail_policy: guardrail_policy.clone(),
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
            workflow_policy,
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

    async fn resolve_artifact_dependencies(
        &self,
        scope: &ActionRuleScope,
        refs: &[(String, ArtifactRef)],
    ) -> Result<Vec<ResolvedArtifactRevisionRef>> {
        let registry = ArtifactRegistry::new(self.pool.clone());
        let mut seen_refs = BTreeSet::new();
        let mut loaded_revisions = BTreeMap::new();
        let mut dependencies = Vec::new();
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
                    )
                    .await?;
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
                    )
                    .await?;
                    dependencies.push(resolved_dependency(artifact_ref, &revision));
                }
                ArtifactRef::Artifact { .. } | ArtifactRef::Tool { .. } => {}
            }
        }
        dependencies.sort_by(|left, right| left.reference.cmp(&right.reference));
        dependencies.dedup_by(|left, right| left.reference == right.reference);
        Ok(dependencies)
    }
}

async fn load_dependency_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    loaded_revisions: &mut BTreeMap<(String, String), StoredArtifactRevision>,
    kind: ArtifactKind,
    name: &str,
    artifact_ref: &ArtifactRef,
) -> Result<StoredArtifactRevision> {
    let key = (kind.as_str().to_string(), name.to_string());
    if let Some(revision) = loaded_revisions.get(&key) {
        return Ok(revision.clone());
    }

    let revision = registry
        .load_visible_published(scope, kind, name)
        .await?
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
    if revision.status != ArtifactStatus::Published {
        return Err(MoaError::ValidationError(format!(
            "agent revision {} must be published before resolution",
            revision.revision_uid
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
        allowed_models: sorted_strings(&definition.allowed_models),
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
    }
}

fn skill_policy_from_definition(definition: &SkillPolicy) -> AgentSkillPolicy {
    AgentSkillPolicy {
        mode: skill_mode_from_definition(definition.mode),
        refs: sorted_ref_strings(&definition.refs),
        max_visible: definition.max_visible,
    }
}

fn workflow_policy_from_definition(definition: &WorkflowPolicy) -> AgentWorkflowPolicy {
    AgentWorkflowPolicy {
        allowed: sorted_ref_strings(&definition.allowed),
    }
}

fn action_policy_from_definition(definition: &ActionPolicy) -> AgentActionPolicy {
    AgentActionPolicy {
        allowed: sorted_ref_strings(&definition.allowed),
        require_admin_review: sorted_ref_strings(&definition.require_admin_review),
    }
}

fn tool_policy_from_definition(definition: &AgentDefinition) -> AgentToolPolicy {
    AgentToolPolicy {
        mode: match definition.tool_policy.mode {
            ToolPolicyMode::Auto => AgentToolPolicyMode::Auto,
            ToolPolicyMode::Allowlist => AgentToolPolicyMode::Allowlist,
            ToolPolicyMode::Denylist => AgentToolPolicyMode::Denylist,
        },
        tools: sorted_strings(&definition.tool_policy.tools),
        denied_tools: sorted_strings(&definition.tool_policy.denied_tools),
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

fn skill_mode_from_definition(mode: SkillPolicyMode) -> AgentSkillPolicyMode {
    match mode {
        SkillPolicyMode::Auto => AgentSkillPolicyMode::Auto,
        SkillPolicyMode::Allowlist => AgentSkillPolicyMode::Allowlist,
        SkillPolicyMode::Pinned => AgentSkillPolicyMode::Pinned,
        SkillPolicyMode::Denylist => AgentSkillPolicyMode::Denylist,
    }
}

fn sorted_ref_strings(refs: &[ArtifactRef]) -> Vec<String> {
    let mut values = refs.iter().map(ToString::to_string).collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn sorted_strings(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

fn locked_tools_from_definition(
    definition: &AgentDefinition,
    refs: &[(String, ArtifactRef)],
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
        .map(|name| LockedToolRef {
            schema_hash: stable_tool_hash(&name, "builtin"),
            name,
            provider: Some("builtin".to_string()),
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools.dedup_by(|left, right| left.name == right.name);
    tools
}

fn stable_tool_hash(name: &str, provider: &str) -> String {
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
        workflow_policy: &'a AgentWorkflowPolicy,
        action_policy: &'a AgentActionPolicy,
        tool_policy: &'a AgentToolPolicy,
        guardrail_policy: &'a AgentGuardrailPolicy,
    }

    let digest = canonical_hash(&HashInput {
        agent_revision_uid,
        artifact_dependencies,
        tool_dependencies,
        instructions: policy.instructions,
        model_policy: policy.model_policy,
        knowledge_policy: policy.knowledge_policy,
        skill_policy: policy.skill_policy,
        workflow_policy: policy.workflow_policy,
        action_policy: policy.action_policy,
        tool_policy: policy.tool_policy,
        guardrail_policy: policy.guardrail_policy,
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
    workflow_policy: &'a AgentWorkflowPolicy,
    action_policy: &'a AgentActionPolicy,
    tool_policy: &'a AgentToolPolicy,
    guardrail_policy: &'a AgentGuardrailPolicy,
}

fn validate_revision_lock(
    revision_lock: &AgentRevisionLock,
    revision: &StoredArtifactRevision,
    policy: &ResolvedHashPolicy<'_>,
) -> Result<()> {
    if revision_lock.agent_revision_uid != revision.revision_uid {
        return Err(MoaError::ValidationError(format!(
            "agent deployment lock points at revision {}, expected {}",
            revision_lock.agent_revision_uid, revision.revision_uid
        )));
    }
    let expected_hash = policy_hash_for(
        revision.revision_uid,
        &revision_lock.artifact_dependencies,
        &revision_lock.tool_dependencies,
        policy,
    )?;
    if revision_lock.canonical_policy_hash != expected_hash {
        return Err(MoaError::ValidationError(format!(
            "agent deployment lock hash mismatch for revision {}",
            revision.revision_uid
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
    use moa_artifacts::agent::{AgentPurpose, ToolPolicy};
    use moa_core::GuardrailMode;

    use super::*;

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

        let locked = locked_tools_from_definition(&definition, &refs);

        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].name, "file_read");
        assert_eq!(locked[0].provider.as_deref(), Some("builtin"));
        assert_eq!(
            locked[0].schema_hash,
            stable_tool_hash("file_read", "builtin")
        );
    }

    fn agent_definition() -> AgentDefinition {
        AgentDefinition {
            display_name: "Support".to_string(),
            purpose: AgentPurpose::default(),
            model_policy: ModelPolicy::default(),
            instruction_policy: Default::default(),
            knowledge_policy: Default::default(),
            skill_policy: Default::default(),
            workflow_policy: Default::default(),
            action_policy: Default::default(),
            tool_policy: Default::default(),
            guardrail_policy: Default::default(),
            revision_note: None,
            metadata: serde_json::json!({}),
        }
    }
}
