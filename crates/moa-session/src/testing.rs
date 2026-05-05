//! Shared Postgres test helpers for MOA crates.

use moa_core::{MoaError, Result};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use crate::PostgresSessionStore;

const DEFAULT_TEST_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:5432/moa";

/// Returns the Postgres URL used by workspace tests.
pub fn test_database_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.to_string())
}

/// Creates a Postgres-backed session store in an isolated schema for tests.
pub async fn create_isolated_test_store() -> Result<(PostgresSessionStore, String, String)> {
    let database_url = test_database_url();
    let schema_name = format!("moa_test_{}", Uuid::now_v7().simple());
    let store = PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?;
    Ok((store, database_url, schema_name))
}

/// Drops one isolated Postgres schema created by `create_isolated_test_store`.
pub async fn cleanup_test_schema(database_url: &str, schema_name: &str) -> Result<()> {
    let pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(1)
        .connect(database_url)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!(
                "failed to connect to Postgres for cleanup: {error}"
            ))
        })?;
    let query = format!(
        "DROP SCHEMA IF EXISTS {} CASCADE",
        quote_identifier(schema_name)
    );
    sqlx::query(&query).execute(&pool).await.map_err(|error| {
        MoaError::StorageError(format!("failed to drop test schema {schema_name}: {error}"))
    })?;
    pool.close().await;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
