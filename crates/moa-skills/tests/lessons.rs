//! Integration tests for skill lesson learning and graph addenda.

mod support;

use moa_core::{MoaError, Result, ScopeContext, ScopedConn};
use moa_memory_graph::GraphStore;
use moa_skills::{LessonContext, NewSkill, SkillRegistry, learn_lesson};
use sqlx::Row;
use uuid::Uuid;

use support::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, graph_store, map_sqlx_error, set_app_role, workspace_scope,
};

#[tokio::test]
async fn learn_lesson_dual_write() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("skills-lesson-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = registry
        .upsert_by_name(NewSkill::from_skill_markdown(
            scope.clone(),
            DISTILLED_SKILL.to_string(),
        ))
        .await?;
    let lesson_ctx = LessonContext::for_app_role(graph_store(store.pool(), &scope));

    let (lesson_uid, addendum_uid) = learn_lesson(
        skill_uid,
        "Do not rotate OAuth refresh-token secrets during active deploys.".to_string(),
        "Avoid refresh-token rotation during active deploys".to_string(),
        scope.clone(),
        Uuid::now_v7(),
        &lesson_ctx,
    )
    .await?;

    let mut conn = ScopedConn::begin(store.pool(), &ScopeContext::from(scope.clone())).await?;
    set_app_role(conn.as_mut()).await?;
    let row = sqlx::query(
        r#"
        SELECT addendum.summary, node.label, node.properties_summary
        FROM moa.skill_addendum addendum
        JOIN moa.node_index node
          ON node.uid = addendum.linked_lesson_uid
        WHERE addendum.addendum_uid = $1
          AND addendum.skill_uid = $2
          AND addendum.linked_lesson_uid = $3
        "#,
    )
    .bind(addendum_uid)
    .bind(skill_uid)
    .bind(lesson_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    assert_eq!(
        row.try_get::<String, _>("summary")
            .map_err(map_sqlx_error)?,
        "Avoid refresh-token rotation during active deploys"
    );
    assert_eq!(
        row.try_get::<String, _>("label").map_err(map_sqlx_error)?,
        "Lesson"
    );
    let properties = row
        .try_get::<serde_json::Value, _>("properties_summary")
        .map_err(map_sqlx_error)?;
    let skill_uid_text = skill_uid.to_string();
    assert_eq!(
        properties
            .get("skill_uid")
            .and_then(serde_json::Value::as_str),
        Some(skill_uid_text.as_str())
    );
    conn.commit().await?;

    lesson_ctx
        .graph()
        .hard_purge(lesson_uid, "redacted:skill-lesson-test")
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let remaining_addenda = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.skill_addendum WHERE addendum_uid = $1",
    )
    .bind(addendum_uid)
    .fetch_one(store.pool())
    .await
    .map_err(map_sqlx_error)?;
    assert_eq!(remaining_addenda, 0);

    sqlx::query("DELETE FROM moa.skill WHERE skill_uid = $1")
        .bind(skill_uid)
        .execute(store.pool())
        .await
        .map_err(map_sqlx_error)?;
    drop(store);
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
