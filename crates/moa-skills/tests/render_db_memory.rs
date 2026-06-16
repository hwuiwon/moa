//! Integration tests for rendering skills with learned lesson addenda.

mod support;

use moa_core::{MoaError, Result, WorkspaceId};
use moa_memory_graph::GraphStore;
use moa_skills::{
    LessonContext, NewSkill, SkillRegistry, SkillRenderContext, learn_lesson, render,
};
use uuid::Uuid;

use support::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, graph_store, map_sqlx_error, workspace_scope,
};

#[tokio::test]
async fn render_with_addenda() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("skills-render-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope.clone(),
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let lesson_ctx = LessonContext::for_app_role(graph_store(store.pool(), &scope));

    let (lesson_uid, _addendum_uid) = learn_lesson(
        skill_uid,
        "When OAuth refresh-token tests fail, inspect deployment-time secret rotation first."
            .to_string(),
        "Check secret rotation before debugging OAuth code".to_string(),
        scope.clone(),
        Uuid::now_v7(),
        &lesson_ctx,
    )
    .await?;
    let skill = registry
        .load_by_name(&scope, "debug-oauth-refresh")
        .await?
        .ok_or_else(|| MoaError::StorageError("skill should exist".to_string()))?;
    let skill_md = registry
        .load_skill_markdown(&scope, skill.skill_uid)
        .await?;
    let rendered = render(
        &skill,
        &skill_md,
        &scope,
        &SkillRenderContext::for_app_role(store.pool().clone()),
    )
    .await?;

    assert!(rendered.starts_with("<!-- learned lessons -->"));
    assert!(rendered.contains("Check secret rotation before debugging OAuth code"));
    assert!(rendered.contains("# Debug OAuth refresh"));

    let loaded = registry
        .load_full(
            &WorkspaceId::new(workspace_name.clone()),
            "debug-oauth-refresh",
        )
        .await?;
    assert!(loaded.contains("Check secret rotation before debugging OAuth code"));

    lesson_ctx
        .graph()
        .hard_purge(lesson_uid, "redacted:skill-render-test")
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    sqlx::query("DELETE FROM moa.skill WHERE skill_uid = $1")
        .bind(skill_uid)
        .execute(store.pool())
        .await
        .map_err(map_sqlx_error)?;
    drop(store);
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
