//! End-to-end coverage of the skill-embedding backfill driver against Postgres.

use async_trait::async_trait;
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_config::EmbeddingBackfillConfig;
use moa_core::error::Result;
use moa_core::traits::EmbeddingProvider;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::identifiers::TenantId;
use moa_skills::embeddings::backfill_skill_embeddings;
use serde_json::json;
use uuid::Uuid;

/// Deterministic 1024-dim embedder derived from input length; no network.
struct OneHotEmbedder;

#[async_trait]
impl EmbeddingProvider for OneHotEmbedder {
    fn model_id(&self) -> &str {
        "test-embedder"
    }

    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(inputs
            .iter()
            .map(|input| {
                let mut vector = vec![0.0_f32; 1024];
                let index = (input.len() % 1023) + 1;
                vector[index] = 1.0;
                vector
            })
            .collect())
    }
}

fn skill_doc(name: &str, description: &str) -> ArtifactDocument {
    let source = json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": { "name": name, "description": description, "tags": ["test"] },
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

async fn publish_skill(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    name: &str,
    description: &str,
) -> Result<()> {
    let document = skill_doc(name, description);
    let source = document.to_yaml().expect("serialize doc");
    let draft = registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", b"# Skill\n".to_vec())],
            },
        )
        .await?;
    registry
        .publish_revision(
            scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn skill_backfill_embeds_then_skips_unchanged_republish_db_memory() -> Result<()> {
    // Pins: the driver embeds a published skill that lacks an embedding, and on a
    // later restale caused by an identity-preserving republish it touches the row
    // (skips the provider call) instead of re-embedding.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let scope = ActionRuleScope::Tenant {
        tenant_id: TenantId::from(Uuid::now_v7()),
    };
    let name = format!("driver-skill-{}", Uuid::now_v7());
    let config = EmbeddingBackfillConfig::default();
    let embedder = OneHotEmbedder;

    publish_skill(&registry, &scope, &name, "backfill target").await?;

    let embedded = backfill_skill_embeddings(&registry, &embedder, &config).await?;
    assert_eq!(embedded, 1, "the newly published skill is embedded once");
    assert!(
        registry
            .list_skills_missing_embedding("test-embedder", 1, 10)
            .await?
            .is_empty(),
        "the skill is no longer missing after the backfill",
    );

    // Republish with the same identity text: this restales the row via
    // updated_at but the identity digest is unchanged.
    publish_skill(&registry, &scope, &name, "backfill target").await?;
    assert_eq!(
        registry
            .list_skills_missing_embedding("test-embedder", 1, 10)
            .await?
            .len(),
        1,
        "an identity-preserving republish restales the row",
    );

    let re_embedded = backfill_skill_embeddings(&registry, &embedder, &config).await?;
    assert_eq!(
        re_embedded, 0,
        "an unchanged republish is touched, not re-embedded",
    );
    assert!(
        registry
            .list_skills_missing_embedding("test-embedder", 1, 10)
            .await?
            .is_empty(),
        "the touch cleared the restaled row",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
