//! Lexical lookup over the `moa.node_index` sidecar.

use moa_core::{ScopeContext, ScopedConn};
use sqlx::PgPool;

use crate::{GraphError, NodeIndexRow};

/// Thin lexical store for NER seed lookup through `name_tsv`.
#[derive(Clone)]
pub struct LexicalStore {
    pool: PgPool,
    scope: Option<ScopeContext>,
    assume_app_role: bool,
}

impl LexicalStore {
    /// Creates an unscoped lexical store.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            scope: None,
            assume_app_role: false,
        }
    }

    /// Creates a lexical store that installs scope GUCs before lookup.
    pub fn scoped(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
        }
    }

    /// Creates a scoped lexical store that assumes `moa_app` inside each transaction.
    pub fn scoped_for_app_role(pool: PgPool, scope: ScopeContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: true,
        }
    }

    /// Looks up seed nodes by name using `plainto_tsquery`.
    pub async fn lookup_seeds(
        &self,
        name: &str,
        limit: i64,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }

        let Some(scope) = &self.scope else {
            return lookup_seed_rows(&self.pool, name, limit).await;
        };

        let mut conn = ScopedConn::begin(&self.pool, scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let rows = crate::node::lookup_seed_by_name(conn.as_mut(), name, limit).await?;
        conn.commit().await?;
        Ok(rows)
    }
}

pub(crate) async fn lookup_seed_rows(
    pool: &PgPool,
    name: &str,
    limit: i64,
) -> Result<Vec<NodeIndexRow>, GraphError> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND name_tsv @@ plainto_tsquery('simple', $1)
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                 last_accessed_at DESC
        LIMIT $2
        "#,
    )
    .bind(name)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(GraphError::from)
}
