//! Lexical lookup over the `moa.node_index` sidecar.

use moa_core::RlsContext;
use moa_db::ScopedConn;
use sqlx::PgPool;

use crate::{GraphError, NodeIndexRow};

/// Thin lexical store for NER seed lookup through `name_tsv`.
#[derive(Clone)]
pub struct LexicalStore {
    pool: PgPool,
    scope: Option<RlsContext>,
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
    pub fn scoped(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: false,
        }
    }

    /// Creates a scoped lexical store that assumes `moa_app` inside each transaction.
    pub fn scoped_for_app_role(pool: PgPool, scope: RlsContext) -> Self {
        Self {
            pool,
            scope: Some(scope),
            assume_app_role: true,
        }
    }

    /// Looks up seed nodes by name using `plainto_tsquery`.
    ///
    /// Results are ordered first by text rank and then by the documented memory rank:
    /// `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`,
    /// where recency decay is `1 / (1 + age_days)` and references are log-normalized up to
    /// 100 references.
    pub async fn lookup_seeds(
        &self,
        name: &str,
        limit: i64,
    ) -> Result<Vec<NodeIndexRow>, GraphError> {
        if limit <= 0 {
            return Ok(Vec::new());
        }

        let Some(scope) = &self.scope else {
            return crate::node::lookup_seed_by_name(&self.pool, name, limit, None).await;
        };

        let mut conn = ScopedConn::begin_as_app(&self.pool, scope, self.assume_app_role).await?;
        let rows = crate::node::lookup_seed_by_name(conn.as_mut(), name, limit, None).await?;
        conn.commit().await?;
        Ok(rows)
    }
}
