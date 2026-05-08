//! Integration tests for graph-backed skill registry behavior.

mod support;

use moa_core::{Result, WorkspaceId};
use moa_skills::{NewSkill, SkillRegistry, parse_skill_markdown};
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
    let skill = parse_skill_markdown(DISTILLED_SKILL)?;
    let registry = SkillRegistry::new(store.pool().clone());
    registry
        .upsert_by_name(NewSkill::from_document(
            scope,
            &skill,
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let skills = registry
        .list_for_pipeline(&WorkspaceId::new(workspace_name))
        .await?;

    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "debug-oauth-refresh");
    assert_eq!(skills[0].estimated_tokens, 900);
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
    let original = parse_skill_markdown(DISTILLED_SKILL)?;
    let first_uid = registry
        .upsert_by_name(NewSkill::from_document(
            scope.clone(),
            &original,
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let second_uid = registry
        .upsert_by_name(NewSkill::from_document(
            scope.clone(),
            &original,
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    assert_eq!(first_uid, second_uid);

    let improved = parse_skill_markdown(IMPROVED_SKILL)?;
    let third_uid = registry
        .upsert_by_name(NewSkill::from_document(
            scope,
            &improved,
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
