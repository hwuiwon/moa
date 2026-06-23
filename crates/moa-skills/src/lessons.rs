//! Skill lesson graph helpers.

use chrono::Utc;
use moa_core::{MemoryScope, MoaError, Result, ScopeContext, ScopedConn};
use moa_memory_graph::{AgeGraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// Context needed to write a learned lesson into the graph.
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

/// Creates a graph `Lesson` node linked to a skill artifact revision.
pub async fn learn_lesson(
    skill_uid: Uuid,
    lesson_text: String,
    summary: String,
    scope: MemoryScope,
    actor: Uuid,
    ctx: &LessonContext,
) -> Result<Uuid> {
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
    let workspace_id = Some(scope_context.tenant_id().to_string());
    let user_id = scope_context
        .contact_id()
        .map(|contact_id| contact_id.to_string());
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

    conn.commit().await?;
    Ok(lesson_uid)
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
