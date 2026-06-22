use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_core::{ActionRuleScope, Result, TenantId};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_preserves_scope_precedence_and_published_revision_history() -> Result<()> {
    // Pins: artifact visibility resolves tenant overrides before workspace defaults.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let name = format!("artifact-scope-{}", Uuid::now_v7());
    let tenant_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };

    let global_doc = skill_doc(&name, "global");
    let global_source = global_doc.to_yaml().expect("serialize global doc");
    let global = registry
        .create_draft(
            &ActionRuleScope::WorkspaceDefault,
            NewArtifactDraft {
                document: &global_doc,
                source_format: "yaml",
                source_text: global_source.as_bytes(),
                files: &[NewArtifactFile::new(
                    "SKILL.md",
                    b"# Global skill\n".to_vec(),
                )],
            },
        )
        .await?;
    registry
        .publish_revision(
            &ActionRuleScope::WorkspaceDefault,
            global.revision_uid,
            &validate_for_status(&global_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_global = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("workspace-default artifact visible to tenant");
    assert_eq!(visible_global.scope, "global");

    let workspace_doc = skill_doc(&name, "workspace-v1");
    let workspace_source = workspace_doc.to_yaml().expect("serialize workspace doc");
    let workspace_v1 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &workspace_doc,
                source_format: "yaml",
                source_text: workspace_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            workspace_v1.revision_uid,
            &validate_for_status(&workspace_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_workspace = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact visible");
    assert_eq!(visible_workspace.scope, "workspace");
    assert_eq!(visible_workspace.version, 1);

    let workspace_v2_doc = skill_doc(&name, "workspace-v2");
    let workspace_v2_source = workspace_v2_doc
        .to_yaml()
        .expect("serialize workspace v2 doc");
    let workspace_v2 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &workspace_v2_doc,
                source_format: "yaml",
                source_text: workspace_v2_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            workspace_v2.revision_uid,
            &validate_for_status(&workspace_v2_doc, ArtifactStatus::Published),
        )
        .await?;
    let visible_workspace_v2 = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact v2 visible");
    assert_eq!(visible_workspace_v2.version, 2);
    assert_eq!(visible_workspace_v2.description, "workspace-v2");
    let loaded_workspace_v1 = registry
        .load_revision(&tenant_scope, workspace_v1.revision_uid)
        .await?
        .expect("tenant v1 remains loadable by exact revision id");
    assert_eq!(loaded_workspace_v1.version, 1);
    assert_eq!(loaded_workspace_v1.status, ArtifactStatus::Published);
    assert_eq!(loaded_workspace_v1.valid_to, None);

    let summaries = registry
        .list_visible(
            &tenant_scope,
            Some(ArtifactKind::Skill),
            Some(ArtifactStatus::Published),
        )
        .await?;
    let matching = summaries
        .iter()
        .filter(|summary| summary.name == name)
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].scope, "workspace");

    let files = registry
        .load_files(&tenant_scope, global.revision_uid)
        .await?;
    assert_eq!(files[0].path, "SKILL.md");

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_persists_behavior_lab_artifact_kinds() -> Result<()> {
    // Pins: the DB registry accepts behavior-lab artifact kinds through the forward constraint.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let workspace_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("checkout-plan-{}", Uuid::now_v7());
    let document = experiment_plan_doc(&name);
    let source = document.to_json().expect("serialize behavior-lab doc");

    let draft = registry
        .create_draft(
            &workspace_scope,
            NewArtifactDraft {
                document: &document,
                source_format: "json",
                source_text: source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    let published = registry
        .publish_revision(
            &workspace_scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await?;

    assert_eq!(published.kind, ArtifactKind::ExperimentPlan);
    assert_eq!(published.status, ArtifactStatus::Published);

    let loaded = registry
        .load_visible_published(&workspace_scope, ArtifactKind::ExperimentPlan, &name)
        .await?
        .expect("published experiment plan is visible");
    assert_eq!(loaded.revision_uid, published.revision_uid);
    assert_eq!(loaded.source_format, "json");
    assert_eq!(loaded.document.kind, ArtifactKind::ExperimentPlan);

    let summaries = registry
        .list_visible(
            &workspace_scope,
            Some(ArtifactKind::ExperimentPlan),
            Some(ArtifactStatus::Published),
        )
        .await?;
    assert_eq!(
        summaries
            .iter()
            .filter(|summary| summary.name == name)
            .count(),
        1
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn skill_doc(name: &str, description: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": {
            "name": name,
            "description": description,
            "tags": ["test"]
        },
        "definition": {
            "type": "skill",
            "spec": {
                "instructions": { "path": "SKILL.md" },
                "inputs": { "type": "object" },
                "outputs": { "type": "object" }
            }
        }
    });
    serde_json::from_value(source).expect("test skill artifact is valid")
}

fn experiment_plan_doc(name: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "experiment_plan",
        "metadata": {
            "name": name,
            "description": "Checkout delay behavior-lab plan",
            "tags": ["behavior-lab"]
        },
        "definition": {
            "type": "experiment_plan",
            "spec": {
                "simulation": {
                    "scenarios": [{
                        "id": "checkout-delay",
                        "initial_situation": "The user asks why checkout is delayed.",
                        "goals": ["Understand the delay and next step."],
                        "success_criteria": ["The target gives a concrete next step."],
                        "failure_criteria": ["The target invents order facts."],
                        "max_turns": 8
                    }],
                    "personas": [{
                        "id": "careful-shopper",
                        "voice": "Patient and precise.",
                        "goals": ["Resolve the delay."],
                        "stop_behavior": "Stop after a concrete next step."
                    }],
                    "profiles": [{
                        "id": "vip-customer",
                        "facts": { "account_tier": "vip" }
                    }]
                },
                "target_variants": [{ "key": "agent-loop", "kind": "agent_loop" }],
                "simulator_model": "gpt-4.1-mini",
                "parallelism": 1,
                "trials_per_combination": 1,
                "budget": { "max_total_cents": 1000 }
            }
        }
    });
    serde_json::from_value(source).expect("test experiment plan artifact is valid")
}
