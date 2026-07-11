//! DB-backed coverage for tenant-configurable agent definition product APIs.

use anyhow::Result;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, StoredArtifactRevision};
use moa_artifacts::validation::validate_for_status;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::agents::{
    AgentDefinitionListRequest, AgentDeployRequest, AgentDeploymentListRequest,
    AgentInstallRequest, AgentInstallationListRequest,
};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_orchestrator::services::agent_definitions::{
    deploy_inner, install_inner, list_definitions_inner, list_deployments_inner,
    list_installations_inner,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn install_deploy_and_list_agent_definitions_use_exact_revision_locks_db_memory() -> Result<()>
{
    // Pins: AgentDefinitions install/deploy moves installation pointers without mutating old revisions.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let agent_name = format!("support-agent-{}", Uuid::now_v7());
    let identity = user_identity();

    let v1 = publish_agent_revision(
        &registry,
        &scope,
        agent_doc(&agent_name, "Support Agent v1", "file_read"),
    )
    .await?;
    let v2 = publish_agent_revision(
        &registry,
        &scope,
        agent_doc(&agent_name, "Support Agent v2", "memory_search"),
    )
    .await?;

    let listed_definitions = map_handler_error(
        list_definitions_inner(
            pool.clone(),
            AgentDefinitionListRequest {
                tenant_id,
                status: None,
            },
        )
        .await,
    )?;
    let listed_agent = listed_definitions
        .agents
        .iter()
        .find(|agent| agent.name == agent_name)
        .expect("latest agent definition should be listed");
    assert_eq!(listed_agent.revision_uid, v2.revision_uid);
    assert_eq!(listed_agent.display_name, "Support Agent v2");

    let installed = map_handler_error(
        install_inner(
            pool.clone(),
            AgentInstallRequest {
                tenant_id,
                revision_uid: v1.revision_uid,
                agent_id: None,
                display_name: None,
                reason: Some("initial deploy".to_string()),
                metadata: json!({ "owner": "support" }),
            },
            identity.clone(),
        )
        .await,
    )?;
    assert_eq!(installed.revision_uid, v1.revision_uid);

    let installations = map_handler_error(
        list_installations_inner(pool.clone(), AgentInstallationListRequest { tenant_id }).await,
    )?;
    assert_eq!(installations.installations.len(), 1);
    assert_eq!(
        installations.installations[0].current_revision_uid,
        Some(v1.revision_uid)
    );

    let deployed = map_handler_error(
        deploy_inner(
            pool.clone(),
            AgentDeployRequest {
                tenant_id,
                installation_uid: installed.installation_uid,
                revision_uid: v2.revision_uid,
                reason: Some("candidate passed simulation".to_string()),
            },
            identity,
        )
        .await,
    )?;
    assert_eq!(deployed.revision_uid, v2.revision_uid);
    assert_ne!(deployed.deployment_uid, installed.deployment_uid);
    assert_ne!(deployed.policy_hash, installed.policy_hash);

    let deployments = map_handler_error(
        list_deployments_inner(
            pool.clone(),
            AgentDeploymentListRequest {
                tenant_id,
                installation_uid: installed.installation_uid,
                limit: None,
            },
        )
        .await,
    )?;
    assert_eq!(deployments.deployments.len(), 2);
    assert!(
        deployments
            .deployments
            .iter()
            .any(|deployment| deployment.revision_uid == v1.revision_uid
                && deployment.status == "superseded"
                && deployment.dependency_lock_hash == installed.policy_hash)
    );
    assert!(
        deployments
            .deployments
            .iter()
            .any(|deployment| deployment.revision_uid == v2.revision_uid
                && deployment.status == "active"
                && deployment.dependency_lock_hash == deployed.policy_hash)
    );

    let loaded_v1 = registry
        .load_revision(&scope, v1.revision_uid)
        .await?
        .expect("v1 remains loadable after deploying v2");
    assert_eq!(loaded_v1.valid_to, None);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

async fn publish_agent_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    document: ArtifactDocument,
) -> Result<StoredArtifactRevision> {
    let report = validate_for_status(&document, ArtifactStatus::Published);
    assert!(
        report.is_ok(),
        "agent fixture should publish cleanly: {report:?}"
    );
    let source = document.to_yaml().expect("serialize agent fixture");
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
    Ok(registry
        .publish_revision(scope, draft.revision_uid, &report)
        .await?)
}

fn agent_doc(name: &str, display_name: &str, allowed_tool: &str) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "agent",
        "metadata": {
            "name": name,
            "description": "Tenant support agent"
        },
        "definition": {
            "type": "agent",
            "spec": {
                "display_name": display_name,
                "purpose": {
                    "summary": "Triage support requests.",
                    "default_task": "Classify support requests.",
                    "expected_outputs": ["classification", "next action"]
                },
                "instruction_policy": {
                    "system_prompt": format!("You are {display_name}.")
                },
                "tool_policy": {
                    "mode": "allowlist",
                    "tools": [allowed_tool]
                },
                "metadata": Value::Null
            }
        }
    }))
    .expect("agent artifact fixture is valid")
}

fn map_handler_error<T>(
    result: std::result::Result<T, restate_sdk::errors::HandlerError>,
) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("{error:?}"))
}

fn user_identity() -> Identity {
    let tenant_id = TenantId::new();
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}
