//! Postgres store used by the durable lineage writer.

/// Postgres connection pool for durable lineage rows and their acceptance queue.
#[derive(Clone)]
pub struct LineageStore {
    postgres: sqlx::PgPool,
}

impl LineageStore {
    /// Builds the production lineage store over Postgres.
    #[must_use]
    pub fn new(postgres: sqlx::PgPool) -> Self {
        Self { postgres }
    }

    /// Returns the Postgres pool.
    #[must_use]
    pub fn postgres(&self) -> &sqlx::PgPool {
        &self.postgres
    }
}
