//! Shared isolated Postgres fixture for auth-provider database tests.

use std::sync::Arc;
use std::time::Duration;

use moa_test_support::fixtures::quote_identifier;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Migrated auth schema with helpers for independent replica pools.
pub(crate) struct TestDatabase {
    database_url: String,
    schema_name: String,
    pool: sqlx::PgPool,
}

impl TestDatabase {
    /// Create and migrate a uniquely named schema for one test.
    pub(crate) async fn new(prefix: &str) -> Self {
        let database_url = test_database_url();
        let schema_name = format!("{prefix}_{}", Uuid::new_v4().simple());
        let pool = connect_pool(&database_url, &schema_name).await;
        moa_migrations::run_auth_schema(&pool, &schema_name)
            .await
            .expect("auth baseline should apply");
        Self {
            database_url,
            schema_name,
            pool,
        }
    }

    /// Return the primary schema-scoped pool behind an `Arc`.
    pub(crate) fn pool(&self) -> Arc<sqlx::PgPool> {
        Arc::new(self.pool.clone())
    }

    /// Borrow the primary schema-scoped pool for direct test setup.
    pub(crate) fn raw_pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// Open a separate pool against the same schema to model another replica.
    pub(crate) async fn independent_pool(&self) -> Arc<sqlx::PgPool> {
        Arc::new(connect_pool(&self.database_url, &self.schema_name).await)
    }
}

async fn connect_pool(database_url: &str, schema_name: &str) -> sqlx::PgPool {
    let search_path = format!("{}, public", quote_identifier(schema_name));
    PgPoolOptions::new()
        .max_connections(3)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("test Postgres should be reachable")
}

fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string())
}
