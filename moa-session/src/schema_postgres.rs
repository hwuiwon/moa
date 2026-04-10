//! Embedded PostgreSQL migrations for the session store.

use moa_core::{MoaError, Result};
use sqlx::PgPool;

/// Runs all embedded PostgreSQL migrations idempotently on the provided pool.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations/postgres")
        .run(pool)
        .await
        .map_err(|error| MoaError::StorageError(format!("postgres migration failed: {error}")))?;
    Ok(())
}
