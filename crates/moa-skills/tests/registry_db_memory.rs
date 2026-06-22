//! Integration tests for graph-backed skill registry behavior.

mod support;

use moa_artifacts::document::{ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
use moa_artifacts::validation::validate_for_status;
use moa_core::{MoaError, Result, WorkspaceId};
use moa_skills::artifact::skill_artifact_document_from_package;
use moa_skills::package::{SkillPackage, SkillPackageFile, ValidatedSkillPackage};
use moa_skills::registry::{NewSkill, SkillRegistry};
use uuid::Uuid;

use support::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, IMPROVED_SKILL, map_sqlx_error, purge_test_skill_name,
    workspace_scope,
};

#[tokio::test]
async fn registry_lists_skill_metadata() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    purge_test_skill_name(&store, "scope-skill").await?;
    let workspace_name = format!("workspace-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope,
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let skills = registry
        .list_for_pipeline(&WorkspaceId::new(workspace_name.clone()))
        .await?;
    let package = registry
        .load_package_by_name(&workspace_scope(&workspace_name), "debug-oauth-refresh")
        .await?
        .expect("stored package exists");

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug-oauth-refresh");
    assert_eq!(skills[0].estimated_tokens, 900);
    assert_eq!(package.skill.file_count, 1);
    assert_eq!(package.files.len(), 1);
    assert_eq!(package.files[0].path, "SKILL.md");
    assert!(
        std::str::from_utf8(&package.files[0].content)
            .expect("stored SKILL.md is UTF-8")
            .contains("# Debug OAuth refresh")
    );
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_upsert_is_idempotent_and_versions_changed_bodies() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    purge_test_skill_name(&store, "scope-skill").await?;
    let workspace_name = format!("workspace-versioned-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let first_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope.clone(),
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let second_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope.clone(),
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    assert_eq!(first_uid, second_uid);

    let third_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope,
            IMPROVED_SKILL.to_string(),
        ))
        .await?;
    assert_ne!(first_uid, third_uid);

    let skills = registry
        .load_for_scope(&workspace_scope(&workspace_name))
        .await?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].skill_uid, third_uid);
    assert_eq!(skills[0].version, 2);
    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let published = artifact_registry
        .load_visible_published(
            &workspace_scope(&workspace_name),
            ArtifactKind::Skill,
            "debug-oauth-refresh",
        )
        .await?
        .expect("direct skill import writes a published artifact");
    assert_eq!(published.status, ArtifactStatus::Published);
    assert_eq!(published.version, 2);
    assert_eq!(
        skill_artifact_revision_count(&store, &workspace_name, "debug-oauth-refresh").await?,
        2
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_loads_published_skill_artifact_without_duplicate_revision() -> Result<()> {
    // Pins: review acceptance publishes one skill artifact revision without writing a legacy mirror.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("workspace-artifact-materialize-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let skill_registry = SkillRegistry::new(store.pool().clone());
    let artifact_registry = ArtifactRegistry::new(store.pool().clone());
    let package = SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()).validate()?;
    let document = skill_artifact_document_from_package(&package, ArtifactStatus::Draft)?;
    let source = document
        .to_yaml()
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let artifact_files = artifact_files_from_package(&package);
    let draft = artifact_registry
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &artifact_files,
            },
        )
        .await?;
    let published = artifact_registry
        .publish_revision(
            &scope,
            draft.revision_uid,
            &validate_for_status(&document, ArtifactStatus::Published),
        )
        .await?;

    let package = skill_registry
        .load_package_by_name(&scope, "debug-oauth-refresh")
        .await?
        .expect("published skill artifact package exists");

    assert_eq!(package.skill.skill_uid, published.revision_uid);
    assert_eq!(package.skill.version, 1);
    assert_eq!(package.files.len(), 1);
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "SKILL.md" && !file.executable)
    );
    assert_eq!(
        skill_artifact_revision_count(&store, &workspace_name, "debug-oauth-refresh").await?,
        1,
        "loading must not insert another published artifact revision"
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

#[tokio::test]
async fn registry_versions_when_supporting_file_changes() -> Result<()> {
    // Pins: changing only a supporting package file creates a new active skill version.
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("workspace-package-files-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());

    let first_uid = registry
        .upsert_by_name(NewSkill::from_package(
            scope.clone(),
            package_with_script(b"printf first\n".to_vec()),
        ))
        .await?;
    let second_uid = registry
        .upsert_by_name(NewSkill::from_package(
            scope.clone(),
            package_with_script(b"printf first\n".to_vec()),
        ))
        .await?;
    assert_eq!(first_uid, second_uid);

    let third_uid = registry
        .upsert_by_name(NewSkill::from_package(
            scope.clone(),
            package_with_script(b"printf second\n".to_vec()),
        ))
        .await?;
    let package = registry
        .load_package_by_name(&scope, "debug-oauth-refresh")
        .await?
        .expect("stored package exists");

    assert_ne!(first_uid, third_uid);
    assert_eq!(package.skill.version, 2);
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "SKILL.md" && !file.executable)
    );
    assert!(
        package
            .files
            .iter()
            .any(|file| file.path == "scripts/run.sh" && file.executable)
    );
    assert_eq!(
        package
            .files
            .iter()
            .find(|file| file.path == "scripts/run.sh")
            .expect("supporting script stored")
            .content,
        b"printf second\n".to_vec()
    );

    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}

fn package_with_script(script: Vec<u8>) -> SkillPackage {
    SkillPackage::new(vec![
        SkillPackageFile::new("scripts/run.sh", script)
            .with_content_type("text/x-shellscript")
            .with_executable(true),
        SkillPackageFile::new("SKILL.md", DISTILLED_SKILL.as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
    ])
}

fn artifact_files_from_package(package: &ValidatedSkillPackage) -> Vec<NewArtifactFile> {
    package
        .files
        .iter()
        .map(|file| NewArtifactFile {
            path: file.path.clone(),
            content: file.content.clone(),
            content_type: file.content_type.clone(),
            executable: file.executable,
        })
        .collect()
}

async fn skill_artifact_revision_count(
    store: &moa_session::PostgresSessionStore,
    workspace_id: &str,
    skill_name: &str,
) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) \
         FROM moa.artifact a \
         JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid \
         WHERE a.workspace_id = $1 AND a.kind = 'skill' AND a.name = $2",
    )
    .bind(workspace_id)
    .bind(skill_name)
    .fetch_one(store.pool())
    .await
    .map_err(map_sqlx_error)
}
