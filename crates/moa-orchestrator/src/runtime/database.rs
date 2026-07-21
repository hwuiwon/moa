//! Database pool and migration wiring for orchestrator startup.

use std::time::Duration;

use anyhow::{Context as AnyhowContext, Result};
use moa_config::MoaConfig;
use sqlx::{PgPool, postgres::PgPoolOptions};

/// Builds the Postgres search path used by runtime and migration pools.
#[must_use]
pub fn database_search_path(config: &MoaConfig) -> String {
    config
        .database
        .schema
        .as_deref()
        .map(|schema_name| format!("{}, public", quote_identifier(schema_name)))
        .unwrap_or_else(|| "public".to_string())
}

/// Builds a Postgres pool with the configured search path applied per connection.
pub async fn build_database_pool(
    database_url: &str,
    database_search_path: &str,
    max_connections: u32,
    connect_timeout: Duration,
) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(connect_timeout)
        .after_connect({
            let database_search_path = database_search_path.to_string();
            move |conn, _meta| {
                let database_search_path = database_search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                        .bind(database_search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(database_url)
        .await
        .map_err(Into::into)
}

/// Applies the shared MOA database migrations.
pub async fn apply_database_migrations(config: &MoaConfig, _pool: &PgPool) -> Result<()> {
    moa_migrations::run(config.database.admin_url())
        .await
        .context("apply database migrations")?;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
