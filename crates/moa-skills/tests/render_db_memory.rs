//! Integration tests for rendering skills with learned graph lessons.

mod support;

use moa_core::{MoaError, Result, TenantId};
use moa_memory_graph::GraphStore;
use moa_skills::lessons::{LessonContext, learn_lesson};
use moa_skills::registry::{NewSkill, SkillRegistry};
use moa_skills::render::{SkillRenderContext, render};
use uuid::Uuid;

use support::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, graph_store, memory_scope, tenant_scope,
};

#[tokio::test]
async fn render_with_graph_lessons() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = Uuid::now_v7().to_string();
    let artifact_scope = tenant_scope(&workspace_name);
    let scope = memory_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            artifact_scope,
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let lesson_ctx = LessonContext::for_app_role(graph_store(store.pool(), &scope));

    let lesson_uid = learn_lesson(
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
        .load_by_name(&tenant_scope(&workspace_name), "debug-oauth-refresh")
        .await?
        .ok_or_else(|| MoaError::StorageError("skill should exist".to_string()))?;
    let skill_md = registry
        .load_skill_markdown(&tenant_scope(&workspace_name), skill.skill_uid)
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
            TenantId::from(Uuid::parse_str(&workspace_name).expect("tenant fixture is a UUID")),
            "debug-oauth-refresh",
        )
        .await?;
    assert!(loaded.contains("Check secret rotation before debugging OAuth code"));

    lesson_ctx
        .graph()
        .hard_purge(lesson_uid, "redacted:skill-render-test")
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    drop(store);
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
