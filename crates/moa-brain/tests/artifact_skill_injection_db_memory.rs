//! DB-backed coverage for artifact skill injection under configured-agent policy.

use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_brain::pipeline::skills::{SELECTED_SKILL_NAMES_METADATA_KEY, SkillInjector};
use moa_core::{
    AgentContext, AgentPolicySnapshot, AgentRevisionLock, AgentSkillPolicy, AgentSkillPolicyMode,
    ContextMessage, ContextProcessor, LockedToolRef, MemoryScope, ModelCapabilities,
    ResolvedArtifactRevisionRef, Result, SessionMeta, UserId, WorkingContext, WorkspaceId,
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn pinned_agent_skill_policy_injects_artifact_revision_files_db_memory() -> Result<()> {
    // Pins: SkillInjector loads exact artifact skill files selected by the session-pinned agent lock.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let workspace_id = WorkspaceId::new(format!("workspace-{}", Uuid::now_v7()));
    let skill_name = format!("artifact-injected-skill-{}", Uuid::now_v7().simple());
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let skill_revision =
        publish_skill_revision(&ArtifactRegistry::new(pool.clone()), &scope, &skill_name).await?;
    let agent_revision_uid = Uuid::now_v7();
    let mut ctx = WorkingContext::new(
        &SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("artifact-skill-user"),
            agent_context: Some(agent_context(
                &skill_name,
                skill_revision.artifact_uid,
                skill_revision.revision_uid,
                skill_revision.version,
                agent_revision_uid,
            )),
            ..SessionMeta::default()
        },
        ModelCapabilities::default(),
    );
    ctx.append_message(ContextMessage::user(
        "Use the configured artifact skill package.",
    ));

    let output = SkillInjector::new(pool).process(&mut ctx).await?;

    assert_eq!(output.items_included, vec![skill_name.clone()]);
    assert_eq!(
        ctx.metadata().get(SELECTED_SKILL_NAMES_METADATA_KEY),
        Some(&json!([skill_name.clone()]))
    );
    let files = ctx.take_trusted_sandbox_files();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].path,
        format!(".moa/skills/{}/SKILL.md", slugify_skill_name(&skill_name))
    );
    assert_eq!(
        String::from_utf8_lossy(&files[0].content),
        "# Artifact Skill\n"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

async fn publish_skill_revision(
    registry: &ArtifactRegistry,
    scope: &MemoryScope,
    skill_name: &str,
) -> Result<moa_artifacts::registry::StoredArtifactRevision> {
    let document = skill_document(skill_name);
    let source = document.to_yaml().expect("serialize skill fixture");
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile {
                    path: "SKILL.md".to_string(),
                    content: b"# Artifact Skill\n".to_vec(),
                    content_type: Some("text/markdown; charset=utf-8".to_string()),
                    executable: false,
                }],
            },
        )
        .await?;
    registry
        .publish_revision(
            scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await
}

fn skill_document(skill_name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": skill_name,
            "description": "Artifact skill injection fixture"
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "allowed_tools": ["file_read"]
            }
        }
    }))
    .expect("skill fixture should be valid")
}

fn agent_context(
    skill_name: &str,
    artifact_uid: Uuid,
    revision_uid: Uuid,
    version: i32,
    agent_revision_uid: Uuid,
) -> AgentContext {
    let dependency = ResolvedArtifactRevisionRef {
        reference: format!("skill://{skill_name}"),
        kind: "skill".to_string(),
        name: skill_name.to_string(),
        artifact_uid,
        revision_uid,
        version,
    };
    let revision_lock = AgentRevisionLock {
        agent_revision_uid,
        artifact_dependencies: vec![dependency.clone()],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            schema_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        canonical_policy_hash: "artifact-skill-injection-policy".to_string(),
    };
    let snapshot = AgentPolicySnapshot {
        skill_policy: AgentSkillPolicy {
            mode: AgentSkillPolicyMode::Pinned,
            refs: vec![format!("skill://{skill_name}")],
            max_visible: Some(1),
        },
        revision_lock: Some(revision_lock),
        ..AgentPolicySnapshot::default()
    };
    AgentContext {
        agent_id: None,
        installation_uid: None,
        deployment_uid: None,
        definition_ref: "agent://artifact-skill-injection".to_string(),
        revision_uid: agent_revision_uid,
        policy_hash: "artifact-skill-injection-policy".to_string(),
        display_name: "Artifact Skill Injection".to_string(),
        artifact_dependencies: vec![dependency],
        tool_dependencies: vec![LockedToolRef {
            name: "file_read".to_string(),
            schema_hash: "file-read-schema".to_string(),
            provider: None,
        }],
        policy_snapshot: serde_json::to_value(snapshot).expect("serialize policy snapshot"),
    }
}

fn slugify_skill_name(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !previous_was_separator && !slug.is_empty() {
            slug.push('-');
            previous_was_separator = true;
        }
    }
    slug.trim_matches('-').to_string()
}
