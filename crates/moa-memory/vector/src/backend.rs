//! Workspace vector-backend selection.

use std::sync::Arc;

use sqlx::PgPool;

use crate::{Error, PgvectorStore, Result, TurbopufferStore, VectorStore};

/// Selects the configured vector store for one workspace.
///
/// Missing `workspace_state` rows default to pgvector. Workspaces explicitly configured for
/// Turbopuffer require a configured client, and HIPAA-tier workspaces additionally require that
/// the client was constructed with BAA enabled.
pub async fn vector_store_for_workspace(
    workspace_id: &str,
    pool: &PgPool,
    pgvector: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
) -> Result<Arc<dyn VectorStore>> {
    let row = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT vector_backend, hipaa_tier
        FROM moa.workspace_state
        WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(pool)
    .await?;

    let (backend, hipaa_tier) =
        row.unwrap_or_else(|| ("pgvector".to_string(), "standard".to_string()));
    resolve_backend_choice(workspace_id, &backend, &hipaa_tier, pgvector, turbopuffer)
}

fn resolve_backend_choice(
    workspace_id: &str,
    backend: &str,
    hipaa_tier: &str,
    pgvector: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
) -> Result<Arc<dyn VectorStore>> {
    match backend {
        "turbopuffer" => {
            let store = turbopuffer.ok_or_else(|| Error::TurbopufferUnavailable {
                workspace_id: workspace_id.to_string(),
            })?;
            if matches!(hipaa_tier, "hipaa" | "restricted") && !store.has_baa() {
                return Err(Error::TurbopufferBaaRequired {
                    workspace_id: workspace_id.to_string(),
                });
            }
            Ok(store)
        }
        _ => Ok(pgvector),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moa_core::{ScopeContext, WorkspaceId};
    use secrecy::SecretString;
    use sqlx::PgPool;

    use crate::{Error, PgvectorStore, TurbopufferStore, backend::resolve_backend_choice};

    fn pg_store() -> Arc<PgvectorStore> {
        Arc::new(PgvectorStore::new(
            PgPool::connect_lazy("postgres://localhost/moa").expect("lazy pool"),
            ScopeContext::workspace(WorkspaceId::new("backend-test")),
        ))
    }

    fn tp_store(baa_enabled: bool) -> Arc<TurbopufferStore> {
        Arc::new(
            TurbopufferStore::new(
                "http://127.0.0.1:1",
                SecretString::from("test-key".to_string()),
                "test",
                baa_enabled,
            )
            .expect("turbopuffer store"),
        )
    }

    #[tokio::test]
    async fn pgvector_selected_by_default() {
        let selected = resolve_backend_choice(
            "w1",
            "pgvector",
            "standard",
            pg_store(),
            Some(tp_store(true)),
        )
        .expect("selection");
        assert_eq!(selected.backend(), "pgvector");
    }

    #[tokio::test]
    async fn turbopuffer_selected_when_configured() {
        let selected = resolve_backend_choice(
            "w1",
            "turbopuffer",
            "standard",
            pg_store(),
            Some(tp_store(false)),
        )
        .expect("selection");
        assert_eq!(selected.backend(), "turbopuffer");
    }

    #[tokio::test]
    async fn hipaa_tier_requires_baa_enabled_turbopuffer_client() {
        let err = match resolve_backend_choice(
            "w1",
            "turbopuffer",
            "hipaa",
            pg_store(),
            Some(tp_store(false)),
        ) {
            Ok(store) => panic!("BAA gate should reject {}", store.backend()),
            Err(error) => error,
        };
        assert!(matches!(err, Error::TurbopufferBaaRequired { .. }));
    }
}
