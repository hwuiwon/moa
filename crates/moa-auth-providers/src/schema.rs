//! Database migration entrypoint for local auth provider storage.
//!
//! SQLx records all crate migration versions in one `_sqlx_migrations` table.
//! This migrator tolerates versions owned by other MOA crates so binaries can
//! apply every crate-owned schema to one database.

use sqlx::PgPool;

use crate::api_keys::ApiKeyError;

/// Apply embedded auth-provider migrations to `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), ApiKeyError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_catalog.set_config('search_path', '\"$user\", public', false)")
        .execute(&mut *conn)
        .await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(&mut *conn).await?;
    Ok(())
}
