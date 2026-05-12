//! Database migration entrypoint for the authorization outbox schema.
//!
//! SQLx stores applied migration versions in one `_sqlx_migrations` table per
//! database. MOA keeps migrations next to their owning crate, so each
//! per-crate migrator must tolerate versions owned by other crates. Binaries
//! should call this module instead of invoking `sqlx::migrate!` directly.

use crate::AuthzError;
use sqlx::PgPool;

/// Apply the embedded authorization migrations to `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), AuthzError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_catalog.set_config('search_path', '\"$user\", public', false)")
        .execute(&mut *conn)
        .await?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(&mut *conn).await?;
    Ok(())
}
