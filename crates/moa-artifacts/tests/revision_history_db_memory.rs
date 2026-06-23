use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft};
use moa_artifacts::validation::validate_for_status;
use moa_core::{ActionRuleScope, Result, TenantId, WorkspaceId};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn agent_revisions_remain_loadable_while_installation_pointer_moves() -> Result<()> {
    // Pins: agent artifact revisions behave like commits while installation current_revision is the deployed ref.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let workspace_id = WorkspaceId::new(format!("workspace-{}", Uuid::now_v7()));
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("support-agent-{}", Uuid::now_v7());

    let v1_doc = agent_doc(&name, "Support Agent", "Triage support requests.");
    let v1_source = v1_doc.to_yaml().expect("serialize v1 agent artifact");
    let v1 = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &v1_doc,
                source_format: "yaml",
                source_text: v1_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let v1 = registry
        .publish_revision(
            &scope,
            v1.revision_uid,
            &validate_for_status(&v1_doc, ArtifactStatus::Published),
        )
        .await?;

    let v2_doc = agent_doc(
        &name,
        "Support Agent v2",
        "Triage support and billing requests.",
    );
    let v2_source = v2_doc.to_yaml().expect("serialize v2 agent artifact");
    let v2 = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &v2_doc,
                source_format: "yaml",
                source_text: v2_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let v2 = registry
        .publish_revision(
            &scope,
            v2.revision_uid,
            &validate_for_status(&v2_doc, ArtifactStatus::Published),
        )
        .await?;

    let loaded_v1 = registry
        .load_revision(&scope, v1.revision_uid)
        .await?
        .expect("v1 revision remains loadable after v2 publish");
    assert_eq!(loaded_v1.kind, ArtifactKind::Agent);
    assert_eq!(loaded_v1.version, 1);
    assert_eq!(loaded_v1.valid_to, None);

    let installation_uid = Uuid::now_v7();
    let deploy_v1_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.agent_installation (
            installation_uid, workspace_id, artifact_uid, definition_ref,
            display_name, current_revision_uid, last_deployment_uid, last_deployed_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, now())
        "#,
    )
    .bind(installation_uid)
    .bind(workspace_id.as_str())
    .bind(v1.artifact_uid)
    .bind(format!("agent://{name}"))
    .bind("Support Agent")
    .bind(v1.revision_uid)
    .bind(deploy_v1_uid)
    .execute(&pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    insert_deployment(
        &pool,
        deploy_v1_uid,
        installation_uid,
        workspace_id.as_str(),
        v1.revision_uid,
        "active",
    )
    .await?;

    let deploy_v2_uid = Uuid::now_v7();
    sqlx::query("UPDATE moa.agent_deployment SET status = 'superseded' WHERE deployment_uid = $1")
        .bind(deploy_v1_uid)
        .execute(&pool)
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    insert_deployment(
        &pool,
        deploy_v2_uid,
        installation_uid,
        workspace_id.as_str(),
        v2.revision_uid,
        "active",
    )
    .await?;
    set_installation_current(&pool, installation_uid, v2.revision_uid, deploy_v2_uid).await?;

    let current_after_deploy = load_current_revision(&pool, installation_uid).await?;
    assert_eq!(current_after_deploy, v2.revision_uid);

    let rollback_uid = Uuid::now_v7();
    sqlx::query("UPDATE moa.agent_deployment SET status = 'rolled_back' WHERE deployment_uid = $1")
        .bind(deploy_v2_uid)
        .execute(&pool)
        .await
        .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    insert_deployment(
        &pool,
        rollback_uid,
        installation_uid,
        workspace_id.as_str(),
        v1.revision_uid,
        "active",
    )
    .await?;
    set_installation_current(&pool, installation_uid, v1.revision_uid, rollback_uid).await?;

    let current_after_rollback = load_current_revision(&pool, installation_uid).await?;
    assert_eq!(current_after_rollback, v1.revision_uid);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn agent_doc(name: &str, display_name: &str, summary: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": {
            "name": name,
            "description": summary,
            "tags": ["support"]
        },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": display_name,
                "purpose": {
                    "summary": summary,
                    "expected_outputs": ["next action"]
                },
                "tool_policy": {
                    "mode": "allowlist",
                    "tools": ["file_read"]
                }
            }
        }
    }))
    .expect("agent artifact fixture is valid")
}

async fn insert_deployment(
    pool: &sqlx::PgPool,
    deployment_uid: Uuid,
    installation_uid: Uuid,
    workspace_id: &str,
    revision_uid: Uuid,
    status: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.agent_deployment (
            deployment_uid, installation_uid, workspace_id, revision_uid,
            status, dependency_lock, dependency_lock_hash
        )
        VALUES ($1, $2, $3, $4, $5, '{}'::JSONB, $6)
        "#,
    )
    .bind(deployment_uid)
    .bind(installation_uid)
    .bind(workspace_id)
    .bind(revision_uid)
    .bind(status)
    .bind(format!("hash-{deployment_uid}"))
    .execute(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn set_installation_current(
    pool: &sqlx::PgPool,
    installation_uid: Uuid,
    revision_uid: Uuid,
    deployment_uid: Uuid,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.agent_installation
        SET current_revision_uid = $2,
            last_deployment_uid = $3,
            last_deployed_at = now(),
            updated_at = now()
        WHERE installation_uid = $1
        "#,
    )
    .bind(installation_uid)
    .bind(revision_uid)
    .bind(deployment_uid)
    .execute(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))?;
    Ok(())
}

async fn load_current_revision(pool: &sqlx::PgPool, installation_uid: Uuid) -> Result<Uuid> {
    sqlx::query_scalar(
        "SELECT current_revision_uid FROM moa.agent_installation WHERE installation_uid = $1",
    )
    .bind(installation_uid)
    .fetch_one(pool)
    .await
    .map_err(|error| moa_core::MoaError::StorageError(error.to_string()))
}
