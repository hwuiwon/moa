//! Analytics service entrypoint used by edge route integration.

use chrono::{DateTime, Utc};
use moa_core::wire::analytics::{
    AnalyticsCatalogResponse, AnalyticsCell, AnalyticsFieldKind, AnalyticsQueryMetadata,
    AnalyticsQueryRequest, AnalyticsQueryResponse,
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::catalog::analytics_catalog;
use crate::clickhouse_exec::AnalyticsClickHouseClient;
use crate::compiler::{AnalyticsBindValue, AnalyticsCompiler, CompiledAnalyticsQuery};
use crate::dialect::AnalyticsBackend;
use crate::error::{Error, Result};

/// Generic analytics service facade.
///
/// A service is bound to one backend at construction: [`AnalyticsService::new`]
/// compiles and executes against Postgres materialized views, while
/// [`AnalyticsService::clickhouse`] compiles and executes against the ClickHouse
/// read models. The edge selects the constructor by `[clickhouse]` presence and
/// calls the matching `query` / `query_clickhouse` entrypoint.
/// Default Postgres `statement_timeout` applied to each analytics query when
/// the caller does not override it from config.
pub(crate) const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone)]
pub struct AnalyticsService {
    compiler: AnalyticsCompiler,
    statement_timeout_ms: u64,
}

impl Default for AnalyticsService {
    fn default() -> Self {
        Self {
            compiler: AnalyticsCompiler::default(),
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
        }
    }
}

impl AnalyticsService {
    /// Creates a Postgres-backed service with the static analytics catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a ClickHouse-backed service with the static analytics catalog.
    pub fn clickhouse() -> Self {
        Self {
            compiler: AnalyticsCompiler::with_backend(
                analytics_catalog(),
                AnalyticsBackend::ClickHouse,
            ),
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
        }
    }

    /// Overrides the Postgres `statement_timeout` applied to each query, in
    /// milliseconds, so a runaway scan is cancelled server-side.
    #[must_use]
    pub fn with_statement_timeout_ms(mut self, statement_timeout_ms: u64) -> Self {
        self.statement_timeout_ms = statement_timeout_ms;
        self
    }

    /// Returns the backend this service compiles and executes against.
    pub fn backend(&self) -> AnalyticsBackend {
        self.compiler.backend()
    }

    /// Returns the analytics catalog available to clients.
    pub fn catalog(&self) -> AnalyticsCatalogResponse {
        analytics_catalog()
    }

    /// Validates and compiles a query without executing it.
    pub fn compile(&self, request: AnalyticsQueryRequest) -> Result<CompiledAnalyticsQuery> {
        self.compiler.compile(request)
    }

    /// Executes a compiled analytics query against Postgres read models.
    pub async fn query(
        &self,
        pool: &PgPool,
        request: AnalyticsQueryRequest,
    ) -> Result<AnalyticsQueryResponse> {
        let compiled = self.compile(request)?;
        let mut conn = ScopedConn::begin_tenant(pool, compiled.effective_tenant_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
        // Bound the database work: an unbounded ordered-percentile scan is
        // cancelled server-side rather than holding a connection open. `SET LOCAL`
        // scopes the timeout to this transaction only.
        sqlx::query("SELECT set_config('statement_timeout', $1, true)")
            .bind(self.statement_timeout_ms.to_string())
            .execute(conn.as_mut())
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
        let rows = execute_compiled(&compiled, conn.as_mut()).await?;
        conn.commit()
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
        let row_count = rows.len() as u64;
        Ok(AnalyticsQueryResponse {
            columns: compiled.columns.clone(),
            rows,
            metadata: AnalyticsQueryMetadata {
                effective_tenant_id: Some(compiled.effective_tenant_id),
                dataset: compiled.dataset,
                row_count,
                read_model_updated_at: None,
            },
        })
    }

    /// Executes a compiled analytics query against the ClickHouse read models.
    ///
    /// The service must have been built with [`AnalyticsService::clickhouse`] so
    /// the compiler emits ClickHouse SQL; the executor pins the tenant on the
    /// driving table exactly as the Postgres path does.
    pub async fn query_clickhouse(
        &self,
        client: &AnalyticsClickHouseClient,
        request: AnalyticsQueryRequest,
    ) -> Result<AnalyticsQueryResponse> {
        let compiled = self.compile(request)?;
        let rows = client.execute(&compiled).await?;
        let row_count = rows.len() as u64;
        Ok(AnalyticsQueryResponse {
            columns: compiled.columns.clone(),
            rows,
            metadata: AnalyticsQueryMetadata {
                effective_tenant_id: Some(compiled.effective_tenant_id),
                dataset: compiled.dataset,
                row_count,
                read_model_updated_at: None,
            },
        })
    }
}

async fn execute_compiled(
    compiled: &CompiledAnalyticsQuery,
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<Vec<AnalyticsCell>>> {
    let mut query = sqlx::query(&compiled.sql);
    for bind in &compiled.bind_values {
        query = match bind {
            AnalyticsBindValue::String(value) => query.bind(value),
            AnalyticsBindValue::Integer(value) => query.bind(*value),
            AnalyticsBindValue::Float(value) => query.bind(*value),
            AnalyticsBindValue::Bool(value) => query.bind(*value),
            AnalyticsBindValue::Json(value) => query.bind(value),
        };
    }

    let rows = query
        .fetch_all(conn)
        .await
        .map_err(|error| Error::Execution(error.to_string()))?;
    rows.iter()
        .map(|row| row_to_cells(row, compiled))
        .collect::<Result<Vec<_>>>()
}

fn row_to_cells(row: &PgRow, compiled: &CompiledAnalyticsQuery) -> Result<Vec<AnalyticsCell>> {
    compiled
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| cell_from_row(row, index, column.kind))
        .collect()
}

fn cell_from_row(row: &PgRow, index: usize, kind: AnalyticsFieldKind) -> Result<AnalyticsCell> {
    match kind {
        AnalyticsFieldKind::String | AnalyticsFieldKind::Uuid => optional_cell(
            row.try_get::<Option<String>, _>(index),
            AnalyticsCell::String,
        ),
        AnalyticsFieldKind::Integer => {
            optional_cell(row.try_get::<Option<i64>, _>(index), |value| {
                AnalyticsCell::Number(serde_json::Number::from(value))
            })
        }
        AnalyticsFieldKind::Float => optional_cell(row.try_get::<Option<f64>, _>(index), |value| {
            serde_json::Number::from_f64(value)
                .map(AnalyticsCell::Number)
                .unwrap_or(AnalyticsCell::Null)
        }),
        AnalyticsFieldKind::Boolean => {
            optional_cell(row.try_get::<Option<bool>, _>(index), AnalyticsCell::Bool)
        }
        AnalyticsFieldKind::Timestamp => {
            optional_cell(row.try_get::<Option<DateTime<Utc>>, _>(index), |value| {
                AnalyticsCell::String(value.to_rfc3339())
            })
        }
        AnalyticsFieldKind::Json => optional_cell(
            row.try_get::<Option<serde_json::Value>, _>(index),
            AnalyticsCell::Json,
        ),
    }
}

fn optional_cell<T>(
    result: std::result::Result<Option<T>, sqlx::Error>,
    wrap: impl FnOnce(T) -> AnalyticsCell,
) -> Result<AnalyticsCell> {
    result
        .map(|value| value.map(wrap).unwrap_or(AnalyticsCell::Null))
        .map_err(|error| Error::Execution(error.to_string()))
}
