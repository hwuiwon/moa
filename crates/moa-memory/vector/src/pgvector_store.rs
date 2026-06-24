//! pgvector-backed graph-memory vector store.

use std::collections::HashMap;

use async_trait::async_trait;
use moa_db::ScopedConn;
use moa_memory_types::ScopeContext;
use pgvector::HalfVector;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    Error, Result, VECTOR_DIMENSION, VectorItem, VectorMatch, VectorQuery, VectorStore, pii_rank,
    validate_dimension,
};

/// pgvector implementation backed by `moa.embeddings`.
#[derive(Clone)]
pub struct PgvectorStore {
    pool: PgPool,
    scope: ScopeContext,
    assume_app_role: bool,
    control_plane: bool,
    exact_search: bool,
}

impl PgvectorStore {
    /// Creates a pgvector store for one request scope.
    pub fn new(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: false,
            control_plane: false,
            exact_search: false,
        }
    }

    /// Creates a pgvector store that sets `moa_app` inside each transaction.
    ///
    /// This is intended for integration tests that connect through the local owner role while
    /// still exercising production RLS policies.
    pub fn new_for_app_role(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: true,
            control_plane: false,
            exact_search: false,
        }
    }

    /// Creates a pgvector store that reads through the workspace control-plane scope.
    ///
    /// This is intended for administrative operations, such as backend promotion,
    /// that must validate both tenant and contact-owned vectors in one workspace.
    pub fn new_for_control_plane(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: false,
            control_plane: true,
            exact_search: false,
        }
    }

    /// Forces exact KNN scans instead of the approximate HNSW index.
    #[must_use]
    pub fn with_exact_search(mut self, exact_search: bool) -> Self {
        self.exact_search = exact_search;
        self
    }

    /// Returns the underlying Postgres pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the request scope used for RLS GUCs.
    pub fn scope(&self) -> &ScopeContext {
        &self.scope
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        let mut conn = if self.control_plane {
            ScopedConn::begin_control_plane(&self.pool).await?
        } else {
            ScopedConn::begin(&self.pool, &self.scope).await?
        };
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        Ok(conn)
    }
}

#[async_trait]
impl VectorStore for PgvectorStore {
    fn backend(&self) -> &'static str {
        "pgvector"
    }

    fn dimension(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn upsert(&self, items: &[VectorItem]) -> Result<()> {
        let mut conn = self.begin().await?;
        guard_workspace_embedder_for_write(conn.as_mut(), items).await?;
        upsert_items(conn.as_mut(), items).await?;
        conn.commit().await?;
        Ok(())
    }

    async fn upsert_in_tx(&self, conn: &mut PgConnection, items: &[VectorItem]) -> Result<()> {
        guard_workspace_embedder_for_write(conn, items).await?;
        upsert_items(conn, items).await
    }

    async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>> {
        let Some(workspace_id) = query.workspace_id.as_deref() else {
            return Err(Error::WorkspaceRequired {
                backend: self.backend(),
                operation: "knn",
            });
        };
        let limit = i64::try_from(query.k).map_err(|_| Error::QueryLimitTooLarge(query.k))?;
        let max_pii_rank = pii_rank(&query.max_pii_class)?;
        if limit <= 0 {
            return Ok(Vec::new());
        }

        let mut conn = self.begin().await?;
        if self.exact_search {
            sqlx::query("SET LOCAL enable_indexscan = off")
                .execute(conn.as_mut())
                .await?;
            sqlx::query("SET LOCAL enable_bitmapscan = off")
                .execute(conn.as_mut())
                .await?;
        }
        guard_workspace_embedder(conn.as_mut(), query).await?;
        validate_dimension(&query.embedding)?;
        let halfvec = HalfVector::from_f32_slice(&query.embedding);
        let mut builder =
            QueryBuilder::<Postgres>::new("SELECT embedding.uid, (1.0 - (embedding.embedding <=> ");
        builder.push_bind(halfvec.clone());
        builder.push(
            r#"))::float4 AS score
               FROM moa.embeddings AS embedding
               JOIN moa.node_index AS node ON node.uid = embedding.uid
               WHERE "#,
        );
        if let Some(as_of) = query.as_of {
            builder.push("node.valid_from <= ");
            builder.push_bind(as_of);
            builder.push(" AND (node.valid_to IS NULL OR node.valid_to > ");
            builder.push_bind(as_of);
            builder.push(") AND (embedding.valid_to IS NULL OR embedding.valid_to > ");
            builder.push_bind(as_of);
            builder.push(")");
        } else {
            builder.push("node.valid_to IS NULL AND embedding.valid_to IS NULL");
        }
        builder.push(" AND embedding.workspace_id = ");
        builder.push_bind(workspace_id);
        builder.push(
            r#"
                 AND CASE embedding.pii_class
                       WHEN 'none' THEN 0
                       WHEN 'pii' THEN 1
                       WHEN 'phi' THEN 2
                       WHEN 'restricted' THEN 3
                       ELSE 4
                     END <= "#,
        );
        builder.push_bind(max_pii_rank);
        if !query.include_global {
            builder.push(" AND embedding.scope <> 'global'");
        }
        if let Some(labels) = query
            .label_filter
            .as_ref()
            .filter(|labels| !labels.is_empty())
        {
            builder.push(" AND embedding.label = ANY(");
            builder.push_bind(labels.as_slice());
            builder.push(")");
        }
        builder.push(" ORDER BY embedding.embedding <=> ");
        builder.push_bind(halfvec);
        builder.push(", embedding.uid ASC");
        builder.push(" LIMIT ");
        builder.push_bind(limit);

        let rows = builder
            .build_query_as::<(Uuid, f32)>()
            .fetch_all(conn.as_mut())
            .await?;
        conn.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(uid, score)| VectorMatch { uid, score })
            .collect())
    }

    async fn delete(&self, uids: &[Uuid]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }

        let mut conn = self.begin().await?;
        delete_items(conn.as_mut(), uids).await?;
        conn.commit().await?;
        Ok(())
    }

    async fn delete_in_tx(&self, conn: &mut PgConnection, uids: &[Uuid]) -> Result<()> {
        delete_items(conn, uids).await
    }
}

struct WorkspaceEmbedderState {
    embedding_model: String,
    embedding_dimension: usize,
    reembed_state: String,
}

async fn guard_workspace_embedder_for_write(
    conn: &mut PgConnection,
    items: &[VectorItem],
) -> Result<()> {
    let mut workspace_ids = Vec::new();
    for workspace_id in items.iter().filter_map(|item| item.workspace_id.as_deref()) {
        if !workspace_ids.iter().any(|seen| seen == workspace_id) {
            workspace_ids.push(workspace_id.to_string());
        }
    }

    let mut states = HashMap::with_capacity(workspace_ids.len());
    for workspace_id in workspace_ids {
        let state = load_workspace_embedder_state(conn, &workspace_id).await?;
        guard_workspace_dimension(&workspace_id, &state)?;
        states.insert(workspace_id, state);
    }

    for item in items {
        let Some(workspace_id) = item.workspace_id.as_deref() else {
            continue;
        };
        let Some(state) = states.get(workspace_id) else {
            return Err(Error::WorkspaceEmbedderStateMissing {
                workspace_id: workspace_id.to_string(),
            });
        };
        if state.embedding_model != item.embedding_model {
            return Err(Error::EmbedderModelMismatch {
                workspace_id: workspace_id.to_string(),
                configured_model: state.embedding_model.clone(),
                requested_model: item.embedding_model.clone(),
            });
        }
    }
    Ok(())
}

async fn guard_workspace_embedder(conn: &mut PgConnection, query: &VectorQuery) -> Result<()> {
    let Some(workspace_id) = query.workspace_id.as_deref() else {
        return Ok(());
    };
    let state = load_workspace_embedder_state(conn, workspace_id).await?;
    if state.reembed_state == "in_progress" {
        return Err(Error::ReembedInProgress {
            workspace_id: workspace_id.to_string(),
        });
    }

    guard_workspace_dimension(workspace_id, &state)
}

async fn load_workspace_embedder_state(
    conn: &mut PgConnection,
    workspace_id: &str,
) -> Result<WorkspaceEmbedderState> {
    let row = sqlx::query(
        r#"
        SELECT embedding_model, embedding_dimension, reembed_state
          FROM moa.workspace_state
         WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(&mut *conn)
    .await?;

    let row = row.ok_or_else(|| Error::WorkspaceEmbedderStateMissing {
        workspace_id: workspace_id.to_string(),
    })?;
    let configured_dimension: i32 = row.try_get("embedding_dimension")?;
    let embedding_dimension = usize::try_from(configured_dimension).unwrap_or_default();
    Ok(WorkspaceEmbedderState {
        embedding_model: row.try_get("embedding_model")?,
        embedding_dimension,
        reembed_state: row.try_get("reembed_state")?,
    })
}

fn guard_workspace_dimension(workspace_id: &str, state: &WorkspaceEmbedderState) -> Result<()> {
    if state.embedding_dimension != VECTOR_DIMENSION {
        return Err(Error::EmbedderMismatch {
            workspace_id: workspace_id.to_string(),
            configured_model: state.embedding_model.clone(),
            configured_dimension: state.embedding_dimension,
            required_dimension: VECTOR_DIMENSION,
        });
    }
    Ok(())
}

async fn upsert_items(conn: &mut PgConnection, items: &[VectorItem]) -> Result<()> {
    for item in items {
        validate_dimension(&item.embedding)?;
        pii_rank(&item.pii_class)?;
        let halfvec = HalfVector::from_f32_slice(&item.embedding);
        sqlx::query(
            r#"
            INSERT INTO moa.embeddings
                (uid, workspace_id, user_id, label, pii_class, embedding,
                 embedding_model, embedding_model_version, valid_to)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (workspace_id, uid) DO UPDATE
                SET user_id = EXCLUDED.user_id,
                    label = EXCLUDED.label,
                    pii_class = EXCLUDED.pii_class,
                    embedding = EXCLUDED.embedding,
                    embedding_model = EXCLUDED.embedding_model,
                    embedding_model_version = EXCLUDED.embedding_model_version,
                    valid_to = EXCLUDED.valid_to
            "#,
        )
        .bind(item.uid)
        .bind(item.workspace_id.as_deref())
        .bind(item.user_id.as_deref())
        .bind(&item.label)
        .bind(&item.pii_class)
        .bind(halfvec)
        .bind(&item.embedding_model)
        .bind(item.embedding_model_version)
        .bind(item.valid_to)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn delete_items(conn: &mut PgConnection, uids: &[Uuid]) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }

    sqlx::query("DELETE FROM moa.embeddings WHERE uid = ANY($1)")
        .bind(uids)
        .execute(conn)
        .await?;
    Ok(())
}
