//! pgvector-backed graph-memory vector store.

use async_trait::async_trait;
use moa_core::{ScopeContext, ScopedConn};
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
}

impl PgvectorStore {
    /// Creates a pgvector store for one request scope.
    pub fn new(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope,
            assume_app_role: false,
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
        }
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
        let mut conn = ScopedConn::begin(&self.pool, &self.scope).await?;
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
        ensure_default_workspace_embedder(conn.as_mut(), items).await?;
        upsert_items(conn.as_mut(), items).await?;
        conn.commit().await?;
        Ok(())
    }

    async fn upsert_in_tx(&self, conn: &mut PgConnection, items: &[VectorItem]) -> Result<()> {
        ensure_default_workspace_embedder(conn, items).await?;
        upsert_items(conn, items).await
    }

    async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>> {
        let limit = i64::try_from(query.k).map_err(|_| Error::QueryLimitTooLarge(query.k))?;
        let max_pii_rank = pii_rank(&query.max_pii_class)?;
        if limit <= 0 {
            return Ok(Vec::new());
        }

        let mut conn = self.begin().await?;
        guard_workspace_embedder(conn.as_mut(), query).await?;
        validate_dimension(&query.embedding)?;
        let halfvec = HalfVector::from_f32_slice(&query.embedding);
        let mut builder = QueryBuilder::<Postgres>::new("SELECT uid, (1.0 - (embedding <=> ");
        builder.push_bind(halfvec.clone());
        builder.push(
            r#"))::float4 AS score
               FROM moa.embeddings
               WHERE valid_to IS NULL
                 AND CASE pii_class
                       WHEN 'none' THEN 0
                       WHEN 'pii' THEN 1
                       WHEN 'phi' THEN 2
                       WHEN 'restricted' THEN 3
                       ELSE 4
                     END <= "#,
        );
        builder.push_bind(max_pii_rank);
        if !query.include_global {
            builder.push(" AND scope <> 'global'");
        }
        if let Some(labels) = query
            .label_filter
            .as_ref()
            .filter(|labels| !labels.is_empty())
        {
            builder.push(" AND label = ANY(");
            builder.push_bind(labels.as_slice());
            builder.push(")");
        }
        builder.push(" ORDER BY embedding <=> ");
        builder.push_bind(halfvec);
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

async fn ensure_default_workspace_embedder(
    conn: &mut PgConnection,
    items: &[VectorItem],
) -> Result<()> {
    for workspace_id in items.iter().filter_map(|item| item.workspace_id.as_deref()) {
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.workspace_state
                (workspace_id, embedding_model, embedding_model_version, embedding_dimension)
            VALUES ($1, 'cohere-embed-v4', 1, $2)
            ON CONFLICT (workspace_id) DO NOTHING
            "#,
        )
        .bind(workspace_id)
        .bind(i32::try_from(VECTOR_DIMENSION).unwrap_or(i32::MAX))
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if inserted > 0 {
            tracing::warn!(
                workspace_id,
                embedding_model = "cohere-embed-v4",
                embedding_dimension = VECTOR_DIMENSION,
                "workspace had no embedder configuration; using default embedder"
            );
        }
    }
    Ok(())
}

async fn guard_workspace_embedder(conn: &mut PgConnection, query: &VectorQuery) -> Result<()> {
    let Some(workspace_id) = query.workspace_id.as_deref() else {
        return Ok(());
    };
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

    let Some(row) = row else {
        tracing::warn!(
            workspace_id,
            embedding_model = "cohere-embed-v4",
            embedding_dimension = VECTOR_DIMENSION,
            "workspace had no embedder configuration; falling back to default embedder"
        );
        return Ok(());
    };

    let reembed_state: String = row.try_get("reembed_state")?;
    if reembed_state == "in_progress" {
        return Err(Error::ReembedInProgress {
            workspace_id: workspace_id.to_string(),
        });
    }

    let configured_model: String = row.try_get("embedding_model")?;
    let configured_dimension: i32 = row.try_get("embedding_dimension")?;
    let configured_dimension = usize::try_from(configured_dimension).unwrap_or(0);
    if configured_dimension != VECTOR_DIMENSION {
        return Err(Error::EmbedderMismatch {
            workspace_id: workspace_id.to_string(),
            configured_model,
            configured_dimension,
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
