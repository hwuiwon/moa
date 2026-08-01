//! Integration tests for skill lesson learning in graph memory.

use moa_core::types::memory::RlsContext;
use moa_core::{error::MoaError, error::Result};
use moa_db::ScopedConn;
use moa_memory_graph::GraphStore;
use moa_skills::lessons::{LessonContext, learn_lesson};
use moa_skills::package::SkillPackage;
use sqlx::Row;
use uuid::Uuid;

use super::skill_graph::{
    DISTILLED_SKILL, GRAPH_TEST_LOCK, graph_store, map_sqlx_error, memory_scope,
    serve_skill_package, set_app_role, tenant_scope,
};

#[tokio::test]
async fn learn_lesson_writes_graph_node() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("skills-lesson-{}", Uuid::now_v7());
    let artifact_scope = tenant_scope(&workspace_name);
    let scope = memory_scope(&workspace_name);
    let skill_uid = serve_skill_package(
        store.pool(),
        artifact_scope,
        SkillPackage::from_skill_markdown(DISTILLED_SKILL.to_string()),
    )
    .await?;
    let lesson_ctx = LessonContext::for_app_role(graph_store(store.pool(), &scope));

    let lesson_uid = learn_lesson(
        skill_uid,
        "Do not rotate OAuth refresh-token secrets during active deploys.".to_string(),
        "Avoid refresh-token rotation during active deploys".to_string(),
        scope.clone(),
        Uuid::now_v7(),
        &lesson_ctx,
    )
    .await?;

    let mut conn = ScopedConn::begin(store.pool(), &RlsContext::from(scope.clone())).await?;
    set_app_role(conn.as_mut()).await?;
    let row = sqlx::query(
        r#"
        SELECT node.label, node.properties_summary
        FROM moa.node_index node
        WHERE node.uid = $1
          AND node.properties_summary->>'skill_uid' = $2
        "#,
    )
    .bind(lesson_uid)
    .bind(skill_uid.to_string())
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
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
    assert_eq!(
        properties
            .get("summary")
            .and_then(serde_json::Value::as_str),
        Some("Avoid refresh-token rotation during active deploys")
    );
    conn.commit().await?;

    lesson_ctx
        .graph()
        .hard_purge(lesson_uid, "redacted:skill-lesson-test")
        .await
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let remaining_lessons =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
            .bind(lesson_uid)
            .fetch_one(store.pool())
            .await
            .map_err(map_sqlx_error)?;
    assert_eq!(remaining_lessons, 0);
    drop(store);
    moa_session::testing::cleanup_test_schema(&database_url, &schema_name).await
}
