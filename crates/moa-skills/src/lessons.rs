//! Skill lesson graph helpers.

use chrono::Utc;
use moa_core::RlsContext;
use moa_core::{MoaError, Result, StoragePartitionId};
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_types::MemoryScope;
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

    let scope_context = RlsContext::from(scope.clone());
    let storage_partition_id =
        Some(StoragePartitionId::for_tenant(scope_context.tenant_id()).to_string());
    let contact_id = scope_context
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
        storage_partition_id: storage_partition_id.clone(),
        contact_id: contact_id.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::TenantId;

    /// Builds a lesson context whose pool is never connected.
    ///
    /// `learn_lesson` validates its text and summary arguments before opening any
    /// connection, so a lazily-constructed pool keeps these guard tests hermetic.
    fn hermetic_lesson_ctx() -> (LessonContext, MemoryScope) {
        let pool = sqlx::PgPool::connect_lazy("postgres://moa:moa@127.0.0.1:1/moa")
            .expect("lazy pool builds without connecting");
        let scope = MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::nil()),
        };
        let graph = AgeGraphStore::scoped_for_app_role(pool, RlsContext::from(scope.clone()));
        (LessonContext::new(graph), scope)
    }

    #[tokio::test]
    async fn learn_lesson_rejects_empty_text() {
        // Pins: a blank lesson body is a validation error before any graph write is attempted.
        let (ctx, scope) = hermetic_lesson_ctx();

        let error = learn_lesson(
            Uuid::now_v7(),
            "   \n".to_string(),
            "Summary of the lesson".to_string(),
            scope,
            Uuid::now_v7(),
            &ctx,
        )
        .await
        .expect_err("empty lesson text must be rejected");

        assert!(
            matches!(error, MoaError::ValidationError(_)),
            "expected a validation error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn learn_lesson_rejects_empty_summary() {
        // Pins: a blank lesson summary is a validation error before any graph write is attempted.
        let (ctx, scope) = hermetic_lesson_ctx();

        let error = learn_lesson(
            Uuid::now_v7(),
            "Do not rotate refresh tokens during active deploys.".to_string(),
            "  ".to_string(),
            scope,
            Uuid::now_v7(),
            &ctx,
        )
        .await
        .expect_err("empty lesson summary must be rejected");

        assert!(
            matches!(error, MoaError::ValidationError(_)),
            "expected a validation error, got {error:?}"
        );
    }
}
