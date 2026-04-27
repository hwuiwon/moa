//! Embedded `PostgreSQL` migrations for the wiki search index.

use moa_core::{MoaError, Result};
use sqlx::{PgPool, raw_sql};

const WIKI_MIGRATIONS: &[&str] = &[
    include_str!("../../moa-session/migrations/postgres/000_scope_helpers.sql"),
    include_str!("../migrations/001_wiki_pages.sql"),
    include_str!("../migrations/002_wiki_embeddings.sql"),
];

/// Runs the wiki search index migrations on the provided pool.
pub async fn migrate(pool: &PgPool, schema_name: Option<&str>) -> Result<()> {
    match schema_name {
        Some(schema_name) => migrate_in_schema(pool, schema_name).await,
        None => {
            for migration in WIKI_MIGRATIONS {
                execute_migration(pool, migration).await?;
            }
            Ok(())
        }
    }
}

async fn migrate_in_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(pool)
    .await
    .map_err(memory_error)?;

    raw_sql("CREATE EXTENSION IF NOT EXISTS pg_trgm; CREATE EXTENSION IF NOT EXISTS vector;")
        .execute(pool)
        .await
        .map_err(memory_error)?;

    let mut tx = pool.begin().await.map_err(memory_error)?;
    let search_path = format!("{}, public", quote_identifier(schema_name));
    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
        .bind(search_path)
        .execute(&mut *tx)
        .await
        .map_err(memory_error)?;

    for migration in WIKI_MIGRATIONS {
        raw_sql(migration)
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "wiki search migration failed for `{schema_name}`: {error}"
                ))
            })?;
    }

    tx.commit().await.map_err(memory_error)?;
    Ok(())
}

async fn execute_migration(pool: &PgPool, sql: &str) -> Result<()> {
    let scoped_sql = format!("SET search_path = public; {sql}");
    raw_sql(&scoped_sql)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(|error| MoaError::StorageError(format!("wiki search migration failed: {error}")))
}

fn memory_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
