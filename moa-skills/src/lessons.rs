//! Skill lesson dual-write helpers.

use chrono::Utc;
use moa_core::{MemoryScope, MoaError, Result, ScopeContext, ScopedConn};
use moa_memory_graph::{AgeGraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// Context needed to write a learned lesson into the graph and skill addendum table.
#[derive(Clone)]
pub struct LessonContext {
    graph: AgeGraphStore,
    assume_app_role: bool,
}

impl LessonContext {
    /// Creates a lesson context backed by an AGE graph store.
    pub fn new(graph: AgeGraphStore) -> Self {
        Self {
            graph,
            assume_app_role: false,
        }
    }

    /// Creates a lesson context that assumes `moa_app` inside each transaction.
    ///
    /// Tests use this when connecting as `moa_owner` while exercising application RLS policies.
    pub fn for_app_role(graph: AgeGraphStore) -> Self {
        Self {
            graph,
            assume_app_role: true,
        }
    }

    /// Returns the graph store used for lesson nodes.
    pub fn graph(&self) -> &AgeGraphStore {
        &self.graph
    }
}

/// Creates a graph `Lesson` node and links it to a skill addendum in one transaction.
pub async fn learn_lesson(
    skill_uid: Uuid,
    lesson_text: String,
    summary: String,
    scope: MemoryScope,
    actor: Uuid,
    ctx: &LessonContext,
) -> Result<(Uuid, Uuid)> {
    if lesson_text.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "lesson text must not be empty".to_string(),
        ));
    }
    if summary.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "lesson summary must not be empty".to_string(),
        ));
    }

    let scope_context = ScopeContext::from(scope.clone());
    let workspace_id = scope_context
        .workspace_id()
        .map(|workspace_id| workspace_id.to_string());
    let user_id = scope_context.user_id().map(|user_id| user_id.to_string());
    let scope_tier = scope_context.tier_str().to_string();
    let mut conn = ScopedConn::begin(ctx.graph.pool(), &scope_context).await?;
    if ctx.assume_app_role {
        set_app_role(conn.as_mut()).await?;
    }

    let lesson_uid = Uuid::now_v7();
    let intent = NodeWriteIntent {
        uid: lesson_uid,
        label: NodeLabel::Lesson,
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
        scope: scope_tier,
        name: lesson_name(&summary),
        properties: json!({
            "text": lesson_text,
            "summary": summary,
            "skill_uid": skill_uid.to_string(),
        }),
        pii_class: PiiClass::None,
        confidence: Some(1.0),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: actor.to_string(),
        actor_kind: "agent".to_string(),
    };
    ctx.graph
        .create_node_in_conn(conn.as_mut(), intent)
        .await
        .map_err(map_graph_error)?;

    let addendum_uid = Uuid::now_v7();
    insert_addendum(
        conn.as_mut(),
        addendum_uid,
        skill_uid,
        lesson_uid,
        workspace_id.as_deref(),
        user_id.as_deref(),
        &summary,
    )
    .await?;

    conn.commit().await?;
    Ok((lesson_uid, addendum_uid))
}

async fn insert_addendum(
    conn: &mut PgConnection,
    addendum_uid: Uuid,
    skill_uid: Uuid,
    lesson_uid: Uuid,
    workspace_id: Option<&str>,
    user_id: Option<&str>,
    summary: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.skill_addendum (
            addendum_uid, skill_uid, linked_lesson_uid, workspace_id, user_id, summary
        )
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(addendum_uid)
    .bind(skill_uid)
    .bind(lesson_uid)
    .bind(workspace_id)
    .bind(user_id)
    .bind(summary)
    .execute(conn)
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

async fn set_app_role(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

fn lesson_name(summary: &str) -> String {
    summary.chars().take(80).collect()
}

fn map_graph_error(error: moa_memory_graph::GraphError) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
