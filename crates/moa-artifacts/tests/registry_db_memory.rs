use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_core::{MemoryScope, Result, UserId, WorkspaceId};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_TEST_POSTGRES_URL, TEST_DATABASE_URL, or DATABASE_URL"]
async fn registry_preserves_scope_precedence_and_published_supersession() -> Result<()> {
    // Pins: artifact visibility uses the same user > workspace > global tiers as skills.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let name = format!("artifact-scope-{}", Uuid::now_v7());
    let workspace_id = WorkspaceId::new(format!("workspace-{}", Uuid::now_v7()));
    let user_id = UserId::new(format!("user-{}", Uuid::now_v7()));
    let workspace_scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let user_scope = MemoryScope::User {
        workspace_id: workspace_id.clone(),
        user_id,
    };

    let global_doc = skill_doc(&name, "global");
    let global_source = global_doc.to_yaml().expect("serialize global doc");
    let global = registry
        .create_draft(
            &MemoryScope::Global,
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
            &MemoryScope::Global,
            global.revision_uid,
            &validate_for_status(&global_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_global = registry
        .load_visible_published(&workspace_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("global artifact visible to workspace");
    assert_eq!(visible_global.scope, "global");

    let workspace_doc = skill_doc(&name, "workspace-v1");
    let workspace_source = workspace_doc.to_yaml().expect("serialize workspace doc");
    let workspace_v1 = registry
        .create_draft(
            &workspace_scope,
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
            &workspace_scope,
            workspace_v1.revision_uid,
            &validate_for_status(&workspace_doc, ArtifactStatus::Published),
        )
        .await?;

    let visible_workspace = registry
        .load_visible_published(&workspace_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("workspace artifact visible");
    assert_eq!(visible_workspace.scope, "workspace");
    assert_eq!(visible_workspace.version, 1);

    let workspace_v2_doc = skill_doc(&name, "workspace-v2");
    let workspace_v2_source = workspace_v2_doc
        .to_yaml()
        .expect("serialize workspace v2 doc");
    let workspace_v2 = registry
        .create_draft(
            &workspace_scope,
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
            &workspace_scope,
            workspace_v2.revision_uid,
            &validate_for_status(&workspace_v2_doc, ArtifactStatus::Published),
        )
        .await?;
    let visible_workspace_v2 = registry
        .load_visible_published(&workspace_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("workspace artifact v2 visible");
    assert_eq!(visible_workspace_v2.version, 2);
    assert_eq!(visible_workspace_v2.description, "workspace-v2");

    let user_doc = skill_doc(&name, "user");
    let user_source = user_doc.to_yaml().expect("serialize user doc");
    let user_revision = registry
        .create_draft(
            &user_scope,
            NewArtifactDraft {
                document: &user_doc,
                source_format: "yaml",
                source_text: user_source.as_bytes(),
                files: &[],
            },
        )
        .await?;
    registry
        .publish_revision(
            &user_scope,
            user_revision.revision_uid,
            &validate_for_status(&user_doc, ArtifactStatus::Published),
        )
        .await?;
    let visible_user = registry
        .load_visible_published(&user_scope, ArtifactKind::Skill, &name)
        .await?
        .expect("user artifact visible");
    assert_eq!(visible_user.scope, "user");

    let summaries = registry
        .list_visible(
            &user_scope,
            Some(ArtifactKind::Skill),
            Some(ArtifactStatus::Published),
        )
        .await?;
    let matching = summaries
        .iter()
        .filter(|summary| summary.name == name)
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].scope, "user");

    let files = registry
        .load_files(&workspace_scope, global.revision_uid)
        .await?;
    assert_eq!(files[0].path, "SKILL.md");

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
