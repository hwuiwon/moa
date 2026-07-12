//! Schema installer for engineering-tier lineage.

use crate::Result;
use crate::error::Error;

/// Ensures the lineage schema exists.
pub async fn ensure_schema(pool: &sqlx::PgPool) -> Result<()> {
    moa_migrations::ensure_lineage_schema(pool)
        .await
        .map_err(|error| Error::Invalid(format!("lineage schema migration failed: {error}")))?;
    Ok(())
}
