//! DB-backed coverage for tenant-configurable agent definition product APIs.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use moa_agents::AgentResolver;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, RecordDecision, ReleaseRepository, StoredArtifactRevision,
    SubmitCandidate,
};
use moa_artifacts::release::{
    ActivationTarget, ActivationTargetClass, CatalogSnapshotBinding, DeterministicVerdict,
    Digest32, EvidenceAdapter, TenantScope,
};
use moa_artifacts::validation::validate_for_status;
use moa_core::traits::{Identity, IdentityType};
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_hands::{ToolRegistry, ToolRouter};
use moa_orchestrator::services::agent_definitions::{
    deploy_inner, install_inner, list_definitions_inner, list_deployments_inner,
    list_installations_inner,
};
use moa_wire::agents::{
    AgentDefinitionListRequest, AgentDeployRequest, AgentDeploymentListRequest,
    AgentInstallRequest, AgentInstallationListRequest,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn install_is_non_serving_and_deploy_requires_an_attestation_db_memory() -> Result<()> {
    // Pins: installing an agent creates a non-serving installation, a ready agent
    // revision alone changes nothing, and only a deploy that spends an activation
    // attestation moves `current_revision_uid`. A deploy without an attestation is
    // refused, and each deploy advances the installation's compare-and-set token.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let pool = store.pool().clone();
    let registry = ArtifactRegistry::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let release_scope = TenantScope::new(tenant_id);
    let agent_name = format!("support-agent-{}", Uuid::now_v7());
    let identity = user_identity();
    let tool_router = Arc::new(ToolRouter::new(
        ToolRegistry::default_local(),
        HashMap::new(),
        moa_hands::local_development_sandbox_policy(),
    ));
    let tool_catalog = tool_router
        .activated_catalog()
        .pin()
        .expect("test deployment catalog should be pinnable");
    crate::artifact_release::seed_environment(
        &pool,
        tenant_id,
        ActivationTargetClass::AgentDeployment,
    )
    .await?;

    let v1 = draft_agent_revision(
        &registry,
        &scope,
        agent_doc(&agent_name, "Support Agent v1", "file_read"),
    )
    .await?;
    let v2 = draft_agent_revision(
        &registry,
        &scope,
        agent_doc(&agent_name, "Support Agent v2", "memory_search"),
    )
    .await?;

    // Drafts are listed as drafts and are not activatable definitions.
    let drafts = map_handler_error(
        list_definitions_inner(
            pool.clone(),
            AgentDefinitionListRequest {
                tenant_id,
                status: Some("draft".to_string()),
            },
        )
        .await,
    )?;
    let listed_agent = drafts
        .agents
        .iter()
        .find(|agent| agent.name == agent_name)
        .expect("the newest draft definition is listed");
    assert_eq!(listed_agent.revision_uid, v2.revision_uid);
    let ready = map_handler_error(
        list_definitions_inner(
            pool.clone(),
            AgentDefinitionListRequest {
                tenant_id,
                status: None,
            },
        )
        .await,
    )?;
    assert!(
        !ready.agents.iter().any(|agent| agent.name == agent_name),
        "an imported agent definition is not activatable yet"
    );

    let installed = map_handler_error(
        install_inner(
            pool.clone(),
            AgentInstallRequest {
                tenant_id,
                revision_uid: v1.revision_uid,
                agent_id: None,
                display_name: None,
                reason: Some("install".to_string()),
                metadata: json!({ "owner": "support" }),
            },
            identity.clone(),
        )
        .await,
    )?;
    assert_eq!(installed.status, "inactive");
    assert_eq!(
        installed.current_revision_uid, None,
        "installing must not deploy"
    );

    let installations = map_handler_error(
        list_installations_inner(pool.clone(), AgentInstallationListRequest { tenant_id }).await,
    )?;
    assert_eq!(installations.installations.len(), 1);
    assert_eq!(installations.installations[0].current_revision_uid, None);

    // A deploy with no attestation for this subject is refused.
    let unattested = deploy_inner(
        pool.clone(),
        tool_catalog.clone(),
        AgentDeployRequest {
            tenant_id,
            installation_uid: installed.installation_uid,
            revision_uid: v1.revision_uid,
            attestation_uid: Uuid::now_v7(),
            reason: Some("no evidence".to_string()),
        },
        None,
        identity.clone(),
    )
    .await;
    assert!(
        unattested.is_err(),
        "an agent revision without a release candidate cannot deploy"
    );

    let v1_attestation = attest_agent_revision(
        &pool,
        release_scope,
        v1.artifact_uid,
        installed.installation_uid,
        v1.revision_uid,
        &tool_router,
    )
    .await?;
    let deployed_v1 = map_handler_error(
        deploy_inner(
            pool.clone(),
            tool_catalog.clone(),
            AgentDeployRequest {
                tenant_id,
                installation_uid: installed.installation_uid,
                revision_uid: v1.revision_uid,
                attestation_uid: v1_attestation,
                reason: Some("initial deploy".to_string()),
            },
            None,
            identity.clone(),
        )
        .await,
    )?;
    assert_eq!(deployed_v1.revision_uid, v1.revision_uid);

    // The same attestation cannot deploy twice.
    let replay = deploy_inner(
        pool.clone(),
        tool_catalog.clone(),
        AgentDeployRequest {
            tenant_id,
            installation_uid: installed.installation_uid,
            revision_uid: v1.revision_uid,
            attestation_uid: v1_attestation,
            reason: Some("replay".to_string()),
        },
        None,
        identity.clone(),
    )
    .await;
    assert!(replay.is_err(), "an attestation is single-use");

    let v2_attestation = attest_agent_revision(
        &pool,
        release_scope,
        v2.artifact_uid,
        installed.installation_uid,
        v2.revision_uid,
        &tool_router,
    )
    .await?;
    let deployed_v2 = map_handler_error(
        deploy_inner(
            pool.clone(),
            tool_catalog,
            AgentDeployRequest {
                tenant_id,
                installation_uid: installed.installation_uid,
                revision_uid: v2.revision_uid,
                attestation_uid: v2_attestation,
                reason: Some("candidate passed evaluation".to_string()),
            },
            None,
            identity,
        )
        .await,
    )?;
    assert_eq!(deployed_v2.revision_uid, v2.revision_uid);
    assert_ne!(deployed_v2.deployment_uid, deployed_v1.deployment_uid);
    assert_ne!(deployed_v2.policy_hash, deployed_v1.policy_hash);

    let pointer_version = sqlx::query_scalar::<_, i64>(
        "SELECT serving_pointer_version FROM moa.agent_installation WHERE installation_uid = $1",
    )
    .bind(installed.installation_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        pointer_version, 2,
        "each attested deploy advances the installation compare-and-set token"
    );

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
                && deployment.dependency_lock_hash == deployed_v1.policy_hash)
    );
    assert!(
        deployments
            .deployments
            .iter()
            .any(|deployment| deployment.revision_uid == v2.revision_uid
                && deployment.status == "active"
                && deployment.dependency_lock_hash == deployed_v2.policy_hash)
    );

    let loaded_v1 = registry
        .load_revision(&scope, v1.revision_uid)
        .await?
        .expect("v1 remains loadable after deploying v2");
    assert_eq!(loaded_v1.valid_to, None);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

/// Submits an agent revision and records a passing deterministic decision.
///
/// Returns the minted attestation, which is what a deploy has to spend.
async fn attest_agent_revision(
    pool: &sqlx::PgPool,
    scope: TenantScope,
    artifact_uid: Uuid,
    installation_uid: Uuid,
    revision_uid: Uuid,
    tool_router: &ToolRouter,
) -> Result<Uuid> {
    // Generic validation is what makes a revision eligible for evaluation, so the
    // fixture records the resolved report exactly as `Artifacts/publish` does.
    let registry = ArtifactRegistry::new(pool.clone());
    let action_scope = scope.action_rule_scope();
    let mut document = registry
        .load_revision(&action_scope, revision_uid)
        .await?
        .expect("candidate revision exists")
        .document;
    document.reference_resolutions =
        moa_artifacts::resolver::ArtifactResolver::new(registry.clone())
            .resolve_document(&action_scope, &document)
            .await?;
    let report = validate_for_status(&document, ArtifactStatus::Ready);
    registry
        .record_validation_report(&action_scope, revision_uid, &report)
        .await?;

    let repository = ReleaseRepository::new(pool.clone());
    let policy = AgentResolver::new(pool.clone())
        .resolve_release_candidate(&action_scope, revision_uid)
        .await?;
    let mut subject_inputs = moa_artifacts::test_fixtures::fixture_subject_inputs();
    subject_inputs.dependency_lock_hash = Digest32(moa_artifacts::canonical::canonical_hash(
        &policy.revision_lock,
    )?);
    let environment = moa_orchestrator::workflows::artifact_release_evaluation::repository::ReleaseEvaluationRepository::new(pool.clone())
        .resolve_subject_environment(scope.tenant_id(), ActivationTargetClass::AgentDeployment)
        .await?;
    subject_inputs.plan = environment.plan;
    subject_inputs.simulator = Some(environment.simulator);
    subject_inputs.tool_bearing = true;
    subject_inputs.tool_catalog = Some(tool_catalog_binding(tool_router)?);
    let target = ActivationTarget::AgentDeployment {
        artifact_uid,
        installation_uid,
    };
    let release_policy = repository.resolve_policy(&scope, target.class()).await?;
    let candidate = repository
        .submit_candidate(SubmitCandidate {
            scope,
            activation_target: target,
            candidate_revision_uid: revision_uid,
            subject_inputs,
            submitted_by: "operator".to_string(),
        })
        .await?
        .candidate;
    let attestation = repository
        .record_decision(RecordDecision {
            scope,
            candidate_revision_uid: revision_uid,
            subject_digest: candidate.subject_digest,
            verdict: DeterministicVerdict::Pass,
            run_uid: Uuid::now_v7(),
            trial_uids: vec![Uuid::now_v7()],
            evidence_ids: vec![Uuid::now_v7()],
            gate_results: std::collections::BTreeMap::from([(
                "result_produced".to_string(),
                "pass".to_string(),
            )]),
            blocking_assertions: release_policy.blocking_assertions,
            evidence_adapter: EvidenceAdapter::BehaviorLabExperiment,
            decided_by: "release-evaluator".to_string(),
        })
        .await?
        .attestation
        .expect("a passing verdict mints an attestation");
    Ok(attestation.attestation_uid)
}

fn tool_catalog_binding(tool_router: &ToolRouter) -> Result<CatalogSnapshotBinding> {
    let pin = tool_router.activated_catalog().pin()?;
    let bytes = hex::decode(pin.contract_hash)?;
    let schema_hash = Digest32::from_slice(&bytes)?;
    let mut snapshot_bytes = [0_u8; 16];
    snapshot_bytes.copy_from_slice(&bytes[..16]);
    snapshot_bytes[6] = (snapshot_bytes[6] & 0x0f) | 0x80;
    snapshot_bytes[8] = (snapshot_bytes[8] & 0x3f) | 0x80;
    Ok(CatalogSnapshotBinding {
        snapshot_uid: Uuid::from_bytes(snapshot_bytes),
        schema_hash,
        activated: true,
    })
}

async fn draft_agent_revision(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    document: ArtifactDocument,
) -> Result<StoredArtifactRevision> {
    let report = validate_for_status(&document, ArtifactStatus::Ready);
    assert!(
        report.is_ok(),
        "agent fixture should be activatable cleanly: {report:?}"
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
    Ok(draft)
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
