use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactFile, ArtifactRegistry, MAX_FILE_SIZE_BYTES, NewArtifactDraft, NewArtifactFile,
};
use moa_artifacts::validation::validate_for_status;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::contact::ContactId, types::identifiers::TenantId,
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_missing_id_lookups_return_none_db_memory() -> Result<()> {
    // Pins: revision and published lookups for unknown ids are Ok(None), not errors.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };

    assert!(
        registry
            .load_revision(&scope, Uuid::now_v7())
            .await?
            .is_none(),
        "unknown revision uid should resolve to None"
    );
    assert!(
        registry
            .load_visible_published(&scope, ArtifactKind::Skill, "does-not-exist")
            .await?
            .is_none(),
        "unknown published artifact should resolve to None"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_round_trips_file_content_type_and_empty_body_db_memory() -> Result<()> {
    // Pins: explicit content_type and executable flags round-trip, and an empty file body persists as zero bytes.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("file-roundtrip-{}", Uuid::now_v7());
    let document = skill_doc(&name, "file round trip");
    let source = document.to_yaml().expect("serialize doc");

    let files = vec![
        NewArtifactFile {
            path: "SKILL.md".to_string(),
            content: b"# Skill\n".to_vec(),
            content_type: Some("text/markdown".to_string()),
            executable: false,
        },
        NewArtifactFile {
            path: "run.sh".to_string(),
            content: b"#!/bin/sh\necho hi\n".to_vec(),
            content_type: Some("application/x-sh".to_string()),
            executable: true,
        },
        NewArtifactFile {
            path: "EMPTY".to_string(),
            content: Vec::new(),
            content_type: Some("application/octet-stream".to_string()),
            executable: false,
        },
    ];

    let draft = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &files,
            },
        )
        .await?;

    let loaded = registry.load_files(&scope, draft.revision_uid).await?;

    let markdown = file_by_path(&loaded, "SKILL.md");
    assert_eq!(markdown.content_type.as_deref(), Some("text/markdown"));
    assert_eq!(markdown.content, b"# Skill\n");
    assert!(!markdown.executable);

    let script = file_by_path(&loaded, "run.sh");
    assert_eq!(script.content_type.as_deref(), Some("application/x-sh"));
    assert!(script.executable);

    let empty = file_by_path(&loaded, "EMPTY");
    assert_eq!(
        empty.content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert!(empty.content.is_empty());
    assert_eq!(empty.file_size_bytes, 0);

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_rejects_oversize_artifact_file_db_memory() -> Result<()> {
    // Pins: a file larger than MAX_FILE_SIZE_BYTES is rejected with a ValidationError and leaves nothing persisted.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("oversize-{}", Uuid::now_v7());
    let document = skill_doc(&name, "oversize file");
    let source = document.to_yaml().expect("serialize doc");
    let oversize = vec![0u8; MAX_FILE_SIZE_BYTES + 1];

    let error = registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("BIG.bin", oversize)],
            },
        )
        .await
        .expect_err("oversize artifact file must reject");
    assert!(
        matches!(error, MoaError::ValidationError(_)),
        "expected ValidationError, got {error:?}"
    );
    assert!(
        error.to_string().contains("too large"),
        "unexpected message: {error}"
    );

    // The failed draft transaction rolled back: nothing for this name is visible.
    assert!(
        registry
            .load_visible(&scope, ArtifactKind::Skill, &name)
            .await?
            .is_none(),
        "rolled-back oversize draft must not persist an artifact"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn file_by_path<'a>(files: &'a [ArtifactFile], path: &str) -> &'a ArtifactFile {
    files
        .iter()
        .find(|file| file.path == path)
        .unwrap_or_else(|| panic!("expected stored file at {path}, got {files:?}"))
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_preserves_tenant_published_revision_history() -> Result<()> {
    // Pins: artifact visibility and revision history are tenant-scoped.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let name = format!("artifact-scope-{}", Uuid::now_v7());
    let tenant_scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };

    let tenant_doc = skill_doc(&name, "tenant-v1");
    let tenant_source = tenant_doc.to_yaml().expect("serialize tenant doc");
    let tenant_v1 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &tenant_doc,
                source_format: "yaml",
                source_text: tenant_source.as_bytes(),
                files: &[NewArtifactFile::new(
                    "SKILL.md",
                    b"# Tenant skill\n".to_vec(),
                )],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            tenant_v1.revision_uid,
            &validate_for_status(&tenant_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_tenant = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact visible");
    assert_eq!(visible_tenant.scope, "tenant");
    assert_eq!(visible_tenant.version, 1);

    let tenant_v2_doc = skill_doc(&name, "tenant-v2");
    let tenant_v2_source = tenant_v2_doc.to_yaml().expect("serialize tenant v2 doc");
    let tenant_v2 = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &tenant_v2_doc,
                source_format: "yaml",
                source_text: tenant_v2_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            tenant_v2.revision_uid,
            &validate_for_status(&tenant_v2_doc, ArtifactStatus::Published),
        )
        .await?;
    let visible_tenant_v2 = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant artifact v2 visible");
    assert_eq!(visible_tenant_v2.version, 2);
    assert_eq!(visible_tenant_v2.description, "tenant-v2");
    let loaded_tenant_v1 = registry
        .load_revision(&tenant_scope, tenant_v1.revision_uid)
        .await?
        .expect("tenant v1 remains loadable by exact revision id");
    assert_eq!(loaded_tenant_v1.version, 1);
    assert_eq!(loaded_tenant_v1.status, ArtifactStatus::Published);
    assert_eq!(loaded_tenant_v1.valid_to, None);

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
    assert_eq!(matching[0].scope, "tenant");

    let files = registry
        .load_files(&tenant_scope, tenant_v1.revision_uid)
        .await?;
    assert_eq!(files[0].path, "SKILL.md");

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn registry_contact_scope_overrides_tenant_without_leaking_to_peers() -> Result<()> {
    // Pins: contact-scoped artifacts are visible only to that contact, while tenant artifacts remain the fallback.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId(Uuid::now_v7());
    let peer_contact_id = ContactId(Uuid::now_v7());
    let tenant_scope = ActionRuleScope::Tenant { tenant_id };
    let contact_scope = ActionRuleScope::Contact {
        tenant_id,
        contact_id,
    };
    let peer_scope = ActionRuleScope::Contact {
        tenant_id,
        contact_id: peer_contact_id,
    };
    let name = format!("artifact-contact-scope-{}", Uuid::now_v7());

    let tenant_doc = skill_doc(&name, "tenant fallback");
    let tenant_source = tenant_doc.to_yaml().expect("serialize tenant doc");
    let tenant_revision = registry
        .create_draft(
            &tenant_scope,
            NewArtifactDraft {
                document: &tenant_doc,
                source_format: "yaml",
                source_text: tenant_source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", b"# Tenant\n".to_vec())],
            },
        )
        .await?;
    registry
        .publish_revision(
            &tenant_scope,
            tenant_revision.revision_uid,
            &validate_for_status(&tenant_doc, ArtifactStatus::Published),
        )
        .await?;

    let contact_before_override = registry
        .load_visible_published(&contact_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("contact should inherit tenant artifact before override");
    assert_eq!(contact_before_override.scope, "tenant");
    assert_eq!(contact_before_override.description, "tenant fallback");

    let contact_doc = skill_doc(&name, "contact override");
    let contact_source = contact_doc.to_yaml().expect("serialize contact doc");
    let contact_revision = registry
        .create_draft(
            &contact_scope,
            NewArtifactDraft {
                document: &contact_doc,
                source_format: "yaml",
                source_text: contact_source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", b"# Contact\n".to_vec())],
            },
        )
        .await?;
    registry
        .publish_revision(
            &contact_scope,
            contact_revision.revision_uid,
            &validate_for_status(&contact_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_contact = registry
        .load_visible_published(&contact_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("contact artifact should be visible to owning contact");
    assert_eq!(visible_contact.scope, "contact");
    assert_eq!(
        visible_contact.user_id.as_ref().map(ToString::to_string),
        Some(contact_id.to_string())
    );
    assert_eq!(visible_contact.description, "contact override");

    let visible_peer = registry
        .load_visible_published(&peer_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("peer contact should still see tenant fallback");
    assert_eq!(visible_peer.scope, "tenant");
    assert_eq!(visible_peer.description, "tenant fallback");

    let visible_tenant = registry
        .load_visible_published(&tenant_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("tenant scope should still see tenant artifact");
    assert_eq!(visible_tenant.scope, "tenant");
    assert_eq!(visible_tenant.description, "tenant fallback");

    let contact_files = registry
        .load_files(&contact_scope, contact_revision.revision_uid)
        .await?;
    assert_eq!(
        file_by_path(&contact_files, "SKILL.md").content,
        b"# Contact\n"
    );

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
