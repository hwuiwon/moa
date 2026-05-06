//! Schema installer for engineering-tier lineage.

use sqlx::Executor;

use crate::Result;

/// Idempotent schema DDL for the engineering-tier lineage tables.
pub const SCHEMA_DDL: &str =
    include_str!("../../../moa-session/migrations/postgres/024_lineage.sql");

/// Ensures the lineage schema exists.
pub async fn ensure_schema(pool: &sqlx::PgPool) -> Result<()> {
    pool.execute(SCHEMA_DDL).await?;
    Ok(())
}
