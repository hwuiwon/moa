//! Skill-identity embedding storage coverage for the artifact registry.

use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, NewSkillEmbedding,
};
use moa_artifacts::validation::validate_for_status;
use moa_core::{
    error::Result, types::action_policy::ActionRuleScope, types::identifiers::TenantId,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Builds a minimal published-skill document with a fixed single tag.
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

/// Publishes one skill revision and returns nothing; the registry tracks it.
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

fn one_hot(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; 1024];
    vector[index] = 1.0;
    vector
}

fn digest(text: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().to_vec()
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn skill_embedding_lists_sets_ranks_and_restales_db_memory() -> Result<()> {
    // Pins: published skills without an embedding are selected, a set clears
    // them, the tenant nearest-neighbor primitive ranks by ascending cosine
    // distance and honors self-exclusion, and a republish restales the row until
    // it is touched or re-embedded.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let tenant = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id: tenant };
    let name_a = format!("alpha-{}", Uuid::now_v7());
    let name_b = format!("beta-{}", Uuid::now_v7());

    publish_skill(&registry, &scope, &name_a, "first skill").await?;
    publish_skill(&registry, &scope, &name_b, "second skill").await?;

    let missing = registry
        .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
        .await?;
    assert_eq!(missing.len(), 2, "both published skills lack an embedding");
    assert!(
        missing
            .iter()
            .all(|row| row.tags == vec!["test".to_string()]),
        "identity tags are surfaced for hashing",
    );
    let row_a = missing
        .iter()
        .find(|row| row.name == name_a)
        .expect("alpha listed")
        .clone();
    let row_b = missing
        .iter()
        .find(|row| row.name == name_b)
        .expect("beta listed")
        .clone();
    let storage_partition = row_a
        .storage_partition_id
        .clone()
        .expect("tenant-scoped skill has a storage partition");

    registry
        .set_skill_embedding(NewSkillEmbedding {
            artifact_uid: row_a.artifact_uid,
            revision_uid: row_a.revision_uid,
            storage_partition_id: row_a.storage_partition_id.as_deref(),
            user_id: row_a.user_id.as_deref(),
            embedding: &one_hot(0),
            model: "mock-embedding-1024",
            model_version: 1,
            source_hash: &digest(&name_a),
            observed_artifact_updated_at: row_a.artifact_updated_at,
        })
        .await?;
    registry
        .set_skill_embedding(NewSkillEmbedding {
            artifact_uid: row_b.artifact_uid,
            revision_uid: row_b.revision_uid,
            storage_partition_id: row_b.storage_partition_id.as_deref(),
            user_id: row_b.user_id.as_deref(),
            embedding: &one_hot(1),
            model: "mock-embedding-1024",
            model_version: 1,
            source_hash: &digest(&name_b),
            observed_artifact_updated_at: row_b.artifact_updated_at,
        })
        .await?;

    assert!(
        registry
            .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
            .await?
            .is_empty(),
        "no skills remain missing after embedding both",
    );

    let neighbors = registry
        .nearest_skill_embeddings(&storage_partition, &one_hot(0), 10, None)
        .await?;
    assert_eq!(
        neighbors.iter().map(|n| n.artifact_uid).collect::<Vec<_>>(),
        vec![row_a.artifact_uid, row_b.artifact_uid],
        "the probe's exact match ranks ahead of the orthogonal skill",
    );
    assert!(neighbors[0].distance < 1e-3, "exact match has ~0 distance");

    let excluded = registry
        .nearest_skill_embeddings(
            &storage_partition,
            &one_hot(0),
            10,
            Some(row_a.artifact_uid),
        )
        .await?;
    assert_eq!(
        excluded.iter().map(|n| n.artifact_uid).collect::<Vec<_>>(),
        vec![row_b.artifact_uid],
        "the excluded artifact is dropped from the ranking",
    );

    // A republish bumps artifact.updated_at past the embedding's updated_at, so
    // the skill restales and is re-selected until touched.
    publish_skill(&registry, &scope, &name_a, "first skill v2").await?;
    let restale = registry
        .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
        .await?;
    assert_eq!(
        restale
            .iter()
            .map(|row| row.artifact_uid)
            .collect::<Vec<_>>(),
        vec![row_a.artifact_uid],
        "the republished skill restales; the untouched one stays embedded",
    );
    let restaled_row = restale
        .iter()
        .find(|row| row.artifact_uid == row_a.artifact_uid)
        .expect("republished alpha listed");

    // Touching with a stale observed timestamp is refused (the artifact moved on),
    // so the guard cannot mask a concurrent identity change.
    assert!(
        !registry
            .touch_skill_embedding(row_a.artifact_uid, row_a.artifact_updated_at)
            .await?,
        "a touch guarded on the pre-republish timestamp does not apply",
    );
    // Touching with the freshly-observed timestamp advances updated_at.
    assert!(
        registry
            .touch_skill_embedding(row_a.artifact_uid, restaled_row.artifact_updated_at)
            .await?,
        "touch advances updated_at for an existing embedding",
    );
    assert!(
        registry
            .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
            .await?
            .is_empty(),
        "touching the restaled embedding clears it without re-embedding",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn skill_embedding_write_refuses_artifact_changed_under_it_db_memory() -> Result<()> {
    // Pins: an identity change that races the (slow) embedding call does not get
    // its stale vector persisted — the write is guarded on the artifact's
    // observed updated_at, so a lost race leaves the row re-selectable instead of
    // stamping a fresh updated_at that would hide the stale vector forever.
    // Mutation guard: dropping `AND a.updated_at = $9` from the write applies the
    // stale vector and fails both the `!applied` and the source-hash assertions.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let tenant = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id: tenant };
    let name = format!("racer-{}", Uuid::now_v7());

    publish_skill(&registry, &scope, &name, "first identity").await?;
    let selected = registry
        .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
        .await?;
    let row = selected.first().expect("skill listed").clone();

    // Embed against the observed timestamp: this applies.
    assert!(
        registry
            .set_skill_embedding(NewSkillEmbedding {
                artifact_uid: row.artifact_uid,
                revision_uid: row.revision_uid,
                storage_partition_id: row.storage_partition_id.as_deref(),
                user_id: row.user_id.as_deref(),
                embedding: &one_hot(0),
                model: "mock-embedding-1024",
                model_version: 1,
                source_hash: &digest("hash-v0"),
                observed_artifact_updated_at: row.artifact_updated_at,
            })
            .await?,
        "the first embedding applies against the observed timestamp",
    );

    // The identity changes during what would have been the provider call.
    publish_skill(&registry, &scope, &name, "second identity").await?;

    // A write still carrying the pre-change timestamp is refused.
    assert!(
        !registry
            .set_skill_embedding(NewSkillEmbedding {
                artifact_uid: row.artifact_uid,
                revision_uid: row.revision_uid,
                storage_partition_id: row.storage_partition_id.as_deref(),
                user_id: row.user_id.as_deref(),
                embedding: &one_hot(1),
                model: "mock-embedding-1024",
                model_version: 1,
                source_hash: &digest("hash-v1-stale"),
                observed_artifact_updated_at: row.artifact_updated_at,
            })
            .await?,
        "a write guarded on the pre-change timestamp does not apply",
    );

    // The row is still selectable and still carries the ORIGINAL embedding's
    // provenance — the stale write left no trace.
    let restale = registry
        .list_skills_missing_embedding("mock-embedding-1024", 1, 10)
        .await?;
    let restaled_row = restale
        .iter()
        .find(|r| r.artifact_uid == row.artifact_uid)
        .expect("the republished skill re-selects as stale");
    assert_eq!(
        restaled_row.stored_source_hash.as_deref(),
        Some(digest("hash-v0").as_slice()),
        "the refused write did not overwrite the stored vector's provenance",
    );

    // A write carrying the current timestamp applies.
    assert!(
        registry
            .set_skill_embedding(NewSkillEmbedding {
                artifact_uid: restaled_row.artifact_uid,
                revision_uid: restaled_row.revision_uid,
                storage_partition_id: restaled_row.storage_partition_id.as_deref(),
                user_id: restaled_row.user_id.as_deref(),
                embedding: &one_hot(1),
                model: "mock-embedding-1024",
                model_version: 1,
                source_hash: &digest("hash-v1"),
                observed_artifact_updated_at: restaled_row.artifact_updated_at,
            })
            .await?,
        "a write carrying the current timestamp applies",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn nearest_skill_embeddings_scoped_filters_by_model_db_memory() -> Result<()> {
    // Pins: the scoped skill NN returns only skills in the requested vector space,
    // while the unscoped entry point compares against every embedding — so an
    // active-model probe never ranks against a previous-space skill vector.
    // Mutation guard: dropping the model predicate from the scoped query makes the
    // other-model skill appear and fails the exclusion assertion.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let tenant = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id: tenant };
    let name_x = format!("scoped-x-{}", Uuid::now_v7());
    let name_y = format!("scoped-y-{}", Uuid::now_v7());

    publish_skill(&registry, &scope, &name_x, "same space").await?;
    publish_skill(&registry, &scope, &name_y, "other space").await?;
    let listed = registry
        .list_skills_missing_embedding("model-x", 1, 10)
        .await?;
    let row_x = listed.iter().find(|r| r.name == name_x).expect("x listed");
    let row_y = listed.iter().find(|r| r.name == name_y).expect("y listed");
    let storage_partition = row_x
        .storage_partition_id
        .clone()
        .expect("tenant-scoped skill has a storage partition");

    // Identical direction, different embedders.
    for (row, model) in [(row_x, "model-x"), (row_y, "model-y")] {
        registry
            .set_skill_embedding(NewSkillEmbedding {
                artifact_uid: row.artifact_uid,
                revision_uid: row.revision_uid,
                storage_partition_id: row.storage_partition_id.as_deref(),
                user_id: row.user_id.as_deref(),
                embedding: &one_hot(0),
                model,
                model_version: 1,
                source_hash: &digest(&row.name),
                observed_artifact_updated_at: row.artifact_updated_at,
            })
            .await?;
    }

    let scoped = registry
        .nearest_skill_embeddings_scoped(
            &storage_partition,
            &one_hot(0),
            10,
            None,
            Some(("model-x", 1)),
        )
        .await?;
    assert_eq!(
        scoped.iter().map(|n| n.artifact_uid).collect::<Vec<_>>(),
        vec![row_x.artifact_uid],
        "Some(model) returns only skills in that vector space",
    );

    let unscoped = registry
        .nearest_skill_embeddings(&storage_partition, &one_hot(0), 10, None)
        .await?;
    assert_eq!(
        unscoped.len(),
        2,
        "the unscoped entry point compares against every embedding",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn skill_backfill_reselects_model_mismatched_rows_db_memory() -> Result<()> {
    // Pins: a skill whose stored vector was produced by a different embedder
    // re-selects for embedding under the active model, so incompatible skill
    // vectors converge instead of being compared against new-space probes.
    // Mutation guard: dropping the model/version predicate from the selection
    // query stops the mismatched row from re-selecting and fails the assertion.
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let registry = ArtifactRegistry::new(store.pool().clone());
    let tenant = TenantId::from(Uuid::now_v7());
    let scope = ActionRuleScope::Tenant { tenant_id: tenant };
    let name = format!("mismatch-{}", Uuid::now_v7());

    publish_skill(&registry, &scope, &name, "converge me").await?;
    let row = registry
        .list_skills_missing_embedding("old-model", 1, 10)
        .await?
        .first()
        .expect("skill listed")
        .clone();
    registry
        .set_skill_embedding(NewSkillEmbedding {
            artifact_uid: row.artifact_uid,
            revision_uid: row.revision_uid,
            storage_partition_id: row.storage_partition_id.as_deref(),
            user_id: row.user_id.as_deref(),
            embedding: &one_hot(0),
            model: "old-model",
            model_version: 1,
            source_hash: &digest("id"),
            observed_artifact_updated_at: row.artifact_updated_at,
        })
        .await?;

    assert!(
        registry
            .list_skills_missing_embedding("old-model", 1, 10)
            .await?
            .is_empty(),
        "a row already in the active space is not re-selected",
    );
    assert_eq!(
        registry
            .list_skills_missing_embedding("new-model", 1, 10)
            .await?
            .iter()
            .map(|r| r.artifact_uid)
            .collect::<Vec<_>>(),
        vec![row.artifact_uid],
        "a model-mismatched row re-selects under the active model",
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
