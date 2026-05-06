//! TimescaleDB schema installer for engineering-tier lineage.

use sqlx::Executor;

use crate::Result;

/// Idempotent schema DDL for the engineering-tier lineage hypertable.
pub const SCHEMA_DDL: &str = include_str!("../sql/schema.sql");

/// Ensures the TimescaleDB lineage schema exists.
pub async fn ensure_schema(pool: &sqlx::PgPool) -> Result<()> {
    pool.execute(SCHEMA_DDL).await?;
    Ok(())
}
