//! SQLx migration entrypoint for OCSF audit tables.

use sqlx::PgPool;

/// Apply `moa-ocsf` database migrations.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_catalog.set_config('search_path', '\"$user\", public', false)")
        .execute(&mut *conn)
        .await?;
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(&mut *conn)
        .await
}
