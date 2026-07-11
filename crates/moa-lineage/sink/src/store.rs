//! Backend selection for durable lineage rows.

use moa_core::config::ClickHouseConfig;

use crate::clickhouse::ClickHouseStore;
use crate::{Result, ensure_schema};

/// Where the lineage writer lands `turn_lineage` rows.
///
/// Postgres always stays attached: `analytics.scores`, dead letters, and the
/// compliance chain state live there under both backends. Selecting
/// [`LineageStore::ClickHouse`] only moves the high-volume `turn_lineage`
/// stream.
#[derive(Clone)]
pub enum LineageStore {
    /// Everything in Postgres/Timescale (the default).
    Postgres(sqlx::PgPool),
    /// `turn_lineage` in ClickHouse; scores, dead letters, and compliance
    /// state stay in Postgres.
    ClickHouse {
        /// ClickHouse row store for `turn_lineage`; boxed to keep the enum
        /// variants size-balanced.
        clickhouse: Box<ClickHouseStore>,
        /// Postgres pool for everything that does not move.
        postgres: sqlx::PgPool,
    },
}

impl LineageStore {
    /// Selects the backend from `[clickhouse]` presence: configured means
    /// ClickHouse, absent means Postgres.
    #[must_use]
    pub fn from_config(clickhouse: Option<&ClickHouseConfig>, postgres: sqlx::PgPool) -> Self {
        match clickhouse {
            Some(config) => Self::ClickHouse {
                clickhouse: Box::new(ClickHouseStore::connect(config)),
                postgres,
            },
            None => Self::Postgres(postgres),
        }
    }

    /// Returns the Postgres pool retained under both backends.
    #[must_use]
    pub fn postgres(&self) -> &sqlx::PgPool {
        match self {
            Self::Postgres(pool) => pool,
            Self::ClickHouse { postgres, .. } => postgres,
        }
    }

    /// Returns the ClickHouse store when that backend is selected.
    #[must_use]
    pub fn clickhouse(&self) -> Option<&ClickHouseStore> {
        match self {
            Self::Postgres(_) => None,
            Self::ClickHouse { clickhouse, .. } => Some(clickhouse),
        }
    }

    /// Short backend label for startup logs and metrics.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Postgres(_) => "postgres",
            Self::ClickHouse { .. } => "clickhouse",
        }
    }

    /// Ensures both the Postgres lineage schema (always required for scores,
    /// dead letters, and compliance state) and the ClickHouse schema when
    /// that backend is selected.
    pub async fn ensure_schema(&self) -> Result<()> {
        ensure_schema(self.postgres()).await?;
        if let Some(clickhouse) = self.clickhouse() {
            clickhouse.ensure_schema().await?;
        }
        Ok(())
    }
}
