use moa_agents::AgentResolver;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::validate_for_status;
use moa_core::{
    ActionRuleScope, AgentRevisionLock, ModelId, Result, SessionActorRef, SessionMeta,
    SessionStore, TenantId, WorkspaceId,
};
use serde_json::json;
use sqlx::types::Json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn installed_agent_resolution_uses_deployment_lock_instead_of_latest_dependency() -> Result<()>
{
    // Pins: deployed agents use exact dependency locks even when referenced artifacts publish newer revisions.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let artifact_resolver = ArtifactResolver::new(ArtifactRegistry::new(pool.clone()));
    let agent_resolver = AgentResolver::new(pool.clone());
    let tenant_id = TenantId::new();
    let workspace_id = WorkspaceId::new(tenant_id.to_string());
    let scope = ActionRuleScope::Tenant { tenant_id };
    let skill_name = format!("support-skill-{}", Uuid::now_v7());
    let agent_name = format!("support-agent-{}", Uuid::now_v7());

    let skill_v1 = publish_document(
        &registry,
        &artifact_resolver,
        &scope,
        skill_doc(&skill_name, "Support policy v1"),
    )
    .await?;
    let agent_revision = publish_document(
        &registry,
        &artifact_resolver,
        &scope,
        agent_doc(&agent_name, &skill_name),
    )
    .await?;
    let deployable_policy = agent_resolver
        .resolve_exact_revision(&scope, agent_revision.revision_uid)
        .await?;

    let installation_uid = Uuid::now_v7();
    let deployment_uid = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    insert_installation(
        &pool,
        &workspace_id,
        installation_uid,
        agent_id,
        &agent_revision,
        deployment_uid,
        &agent_name,
    )
    .await?;
    insert_deployment(
        &pool,
        &workspace_id,
        installation_uid,
        deployment_uid,
        agent_revision.revision_uid,
        &deployable_policy.revision_lock,
    )
    .await?;

    let _skill_v2 = publish_document(
        &registry,
        &artifact_resolver,
        &scope,
        skill_doc(&skill_name, "Support policy v2"),
    )
    .await?;

    let resolved = agent_resolver
        .resolve_installation(&scope, installation_uid)
        .await?;
    assert_eq!(resolved.agent_context.agent_id, Some(agent_id));
    assert_eq!(
        resolved.agent_context.installation_uid,
        Some(installation_uid)
    );
    assert_eq!(resolved.agent_context.deployment_uid, Some(deployment_uid));
    assert_eq!(
        resolved.agent_context.revision_uid,
        agent_revision.revision_uid
    );
    assert_eq!(
        resolved.revision_lock.agent_revision_uid,
        agent_revision.revision_uid
    );
    assert_eq!(resolved.revision_lock.artifact_dependencies.len(), 1);
    assert_eq!(
        resolved.revision_lock.artifact_dependencies[0].revision_uid,
        skill_v1.revision_uid
    );
    assert_eq!(
        resolved.agent_context.artifact_dependencies[0].revision_uid,
        skill_v1.revision_uid
    );
    assert!(resolved.tool_policy.allows("file_read"));
    assert!(!resolved.tool_policy.allows("shell"));
    assert!(!resolved.tool_policy.allows("network_fetch"));
    assert!(resolved.agent_context.allows_tool("file_read")?);
    assert!(!resolved.agent_context.allows_tool("network_fetch")?);

    let session_id = store
        .create_session(SessionMeta {
            tenant_id,
            created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
            model: ModelId::new("test-model"),
            agent_context: Some(resolved.agent_context.clone()),
            ..SessionMeta::default()
        })
        .await?;
    let loaded_session = store.get_session(session_id).await?;
    assert_eq!(
        loaded_session
            .agent_context
            .as_ref()
            .expect("session should load pinned agent context")
            .policy_hash,
        resolved.agent_context.policy_hash
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

async fn publish_document(
    registry: &ArtifactRegistry,
    artifact_resolver: &ArtifactResolver,
    scope: &ActionRuleScope,
    mut document: ArtifactDocument,
) -> Result<StoredArtifactRevision> {
    document.reference_resolutions = artifact_resolver.resolve_document(scope, &document).await?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    assert!(
        report.is_ok(),
        "artifact fixture should publish cleanly: {report:?}"
    );
    let source = document.to_yaml().expect("serialize artifact fixture");
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(scope, draft.revision_uid, &report)
        .await
}

fn skill_doc(name: &str, description: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": name,
            "description": description
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": {
                    "path": "SKILL.md"
                },
                "allowed_tools": ["file_read"]
            }
        }
    }))
    .expect("skill artifact fixture is valid")
}

fn agent_doc(name: &str, skill_name: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": {
            "name": name,
            "description": "Tenant support triage agent"
        },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": "Support Triage",
                "purpose": {
                    "summary": "Triage support requests.",
                    "default_task": "Classify the request and suggest the next action.",
                    "expected_outputs": ["classification", "next action"]
                },
                "instruction_policy": {
                    "system_prompt": "You are the tenant support triage agent.",
                    "instructions": ["Stay within the configured support policy."]
                },
                "skill_policy": {
                    "mode": "pinned",
                    "refs": [format!("skill://{skill_name}")]
                },
                "tool_policy": {
                    "mode": "allowlist",
                    "tools": ["file_read"],
                    "denied_tools": ["shell"]
                }
            }
        }
    }))
    .expect("agent artifact fixture is valid")
}

async fn insert_installation(
    pool: &sqlx::PgPool,
    workspace_id: &WorkspaceId,
    installation_uid: Uuid,
    agent_id: Uuid,
    revision: &StoredArtifactRevision,
    deployment_uid: Uuid,
    agent_name: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.agent_installation (
            installation_uid, workspace_id, agent_id, artifact_uid, definition_ref,
            display_name, current_revision_uid, last_deployment_uid, last_deployed_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())
        "#,
    )
    .bind(installation_uid)
    .bind(workspace_id.as_str())
    .bind(agent_id)
    .bind(revision.artifact_uid)
    .bind(format!("agent://{agent_name}"))
    .bind("Support Triage")
    .bind(revision.revision_uid)
    .bind(deployment_uid)
    .execute(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn insert_deployment(
    pool: &sqlx::PgPool,
    workspace_id: &WorkspaceId,
    installation_uid: Uuid,
    deployment_uid: Uuid,
    revision_uid: Uuid,
    revision_lock: &AgentRevisionLock,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.agent_deployment (
            deployment_uid, installation_uid, workspace_id, revision_uid,
            status, dependency_lock, dependency_lock_hash
        )
        VALUES ($1, $2, $3, $4, 'active', $5, $6)
        "#,
    )
    .bind(deployment_uid)
    .bind(installation_uid)
    .bind(workspace_id.as_str())
    .bind(revision_uid)
    .bind(Json(revision_lock))
    .bind(&revision_lock.canonical_policy_hash)
    .execute(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(())
}
