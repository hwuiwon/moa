//! Integration tests for graph-backed skill registry behavior.

mod support;

use moa_core::{Result, WorkspaceId};
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::registry::{NewSkill, SkillRegistry};
use uuid::Uuid;

use support::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, IMPROVED_SKILL, purge_test_skill_name, workspace_scope,
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
    assert_eq!(skills[0].previous_skill_uid, Some(first_uid));

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
    assert_eq!(package.skill.previous_skill_uid, Some(first_uid));
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
