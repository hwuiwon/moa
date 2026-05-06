//! Integration tests for skill parsing, registry loading, distillation, and improvement.

use memory_graph::{AgeGraphStore, GraphStore};
use moa_core::{MoaError, Result, ScopeContext, ScopedConn, WorkspaceId};
use moa_skills::{
    LessonContext, NewSkill, SkillRegistry, SkillRenderContext, learn_lesson, parse_skill_markdown,
    render,
};
use sqlx::{PgConnection, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static GRAPH_TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn workspace_scope(workspace_id: &str) -> moa_core::MemoryScope {
    moa_core::MemoryScope::Workspace {
        workspace_id: WorkspaceId::new(workspace_id),
    }
}

fn graph_store(pool: &sqlx::PgPool, scope: &moa_core::MemoryScope) -> AgeGraphStore {
    AgeGraphStore::scoped_for_app_role(pool.clone(), ScopeContext::from(scope.clone()))
}

async fn set_app_role(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

const DISTILLED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search
metadata:
  moa-version: "1.0"
  moa-one-liner: "Repeatable OAuth refresh-token debugging workflow"
  moa-tags: "oauth, auth, debugging"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:00:00Z"
  moa-auto-generated: "true"
  moa-source-session: "session-1"
  moa-use-count: "0"
  moa-last-used: "2026-04-09T16:00:00Z"
  moa-success-rate: "1.0"
  moa-brain-affinity: "coding"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "900"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Inspect the refresh-token path.
3. Verify the fix.
"#;

const IMPROVED_SKILL: &str = r#"---
name: debug-oauth-refresh
description: "Investigate and fix OAuth refresh-token bugs with regression checks"
compatibility: "Requires local repo access"
allowed-tools: bash file_read file_search file_write
metadata:
  moa-version: "1.0"
  moa-one-liner: "Repeatable OAuth refresh-token debugging workflow with regression checks"
  moa-tags: "oauth, auth, debugging"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:30:00Z"
  moa-auto-generated: "true"
  moa-source-session: "session-2"
  moa-use-count: "0"
  moa-last-used: "2026-04-09T16:30:00Z"
  moa-success-rate: "1.0"
  moa-brain-affinity: "coding"
  moa-sandbox-tier: "container"
  moa-estimated-tokens: "950"
---

# Debug OAuth refresh

1. Reproduce the bug.
2. Add a regression test before changing code.
3. Inspect the refresh-token path.
4. Verify the fix and the new test.
"#;

#[test]
fn parses_skill_markdown() {
    let skill = parse_skill_markdown(DISTILLED_SKILL).unwrap();

    assert_eq!(skill.frontmatter.name, "debug-oauth-refresh");
    assert_eq!(skill.frontmatter.estimated_tokens(&skill.body), 900);
}

#[tokio::test]
async fn registry_lists_skill_metadata() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
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

#[tokio::test]
async fn learn_lesson_dual_write() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("skills-lesson-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let skill_doc = parse_skill_markdown(DISTILLED_SKILL)?;
    let registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = registry
        .upsert_by_name(NewSkill::from_document(
            scope.clone(),
            &skill_doc,
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

#[tokio::test]
async fn render_with_addenda() -> Result<()> {
    let _guard = GRAPH_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) =
        moa_session::testing::create_isolated_test_store().await?;
    let workspace_name = format!("skills-render-{}", Uuid::now_v7());
    let scope = workspace_scope(&workspace_name);
    let skill_doc = parse_skill_markdown(DISTILLED_SKILL)?;
    let registry = SkillRegistry::new(store.pool().clone());
    let skill_uid = registry
        .upsert_by_name(NewSkill::from_document(
            scope.clone(),
            &skill_doc,
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
    let rendered = render(
        &skill,
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
