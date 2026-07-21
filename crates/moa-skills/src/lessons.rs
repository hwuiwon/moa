//! Skill lesson graph helpers.

use chrono::Utc;
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{error::MoaError, error::Result, types::identifiers::StoragePartitionId};
use moa_db::ScopedConn;
use moa_memory_graph::{NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use serde_json::json;
use uuid::Uuid;

use crate::util::set_app_role;

/// Context needed to write a learned lesson into the graph.
#[derive(Clone)]
pub struct LessonContext {
    graph: PostgresGraphStore,
    assume_app_role: bool,
}

impl LessonContext {
    /// Creates a lesson context backed by a Postgres graph store.
    pub fn new(graph: PostgresGraphStore) -> Self {
        Self {
            graph,
            assume_app_role: false,
        }
    }

    /// Creates a lesson context that assumes `moa_app` inside each transaction.
    ///
    /// Tests use this when connecting as `moa_owner` while exercising application RLS policies.
    pub fn for_app_role(graph: PostgresGraphStore) -> Self {
        Self {
            graph,
            assume_app_role: true,
        }
    }

    /// Returns the graph store used for lesson nodes.
    pub fn graph(&self) -> &PostgresGraphStore {
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
        barrier: None,
        uid: lesson_uid,
        data_subject_id: scope_context
            .contact_id()
            .map_or(scope_context.tenant_id().0, |contact_id| contact_id.0),
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
        pii_class: SensitivityClass::None,
        confidence: Some(1.0),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
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

fn lesson_name(summary: &str) -> String {
    summary.chars().take(80).collect()
}

fn map_graph_error(error: moa_memory_graph::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::identifiers::TenantId;
    use moa_crypto::LocalKmsProvider;
    use std::sync::Arc;

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
        let graph = PostgresGraphStore::scoped_for_app_role(
            pool,
            RlsContext::from(scope.clone()),
            Arc::new(LocalKmsProvider::new()),
        );
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
