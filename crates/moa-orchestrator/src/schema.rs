//! Database migration entrypoint for orchestrator-owned tables.
//!
//! MOA keeps migrations next to their owning crate. This migrator tolerates
//! rows in `_sqlx_migrations` owned by other crates so the orchestrator binary
//! can apply all crate-owned schemas to one database.

use sqlx::PgPool;

/// Apply embedded orchestrator migrations to `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_catalog.set_config('search_path', '\"$user\", public', false)")
        .execute(&mut *conn)
        .await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(&mut *conn).await
}
