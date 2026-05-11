//! Database migration entrypoint for Auth0/OIDC provider storage.
//!
//! SQLx records all crate migration versions in one `_sqlx_migrations` table.
//! This migrator tolerates versions owned by other MOA crates so binaries can
//! apply every crate-owned schema to one database.

use sqlx::PgPool;

/// Apply embedded Auth0/OIDC provider migrations to `pool`.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await
}
