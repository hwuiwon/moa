//! Analytics service entrypoint used by edge route integration.

use chrono::{DateTime, Utc};
use moa_core::wire::analytics::{
    AnalyticsCatalogResponse, AnalyticsCell, AnalyticsFieldKind, AnalyticsQueryMetadata,
    AnalyticsQueryRequest, AnalyticsQueryResponse,
};
use moa_db::ScopedConn;
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::catalog::analytics_catalog;
use crate::compiler::{AnalyticsBindValue, AnalyticsCompiler, CompiledAnalyticsQuery};
use crate::error::{AnalyticsError, Result};

/// Generic analytics service facade.
#[derive(Debug, Clone, Default)]
pub struct AnalyticsService {
    compiler: AnalyticsCompiler,
}

impl AnalyticsService {
    /// Creates a service with the default static analytics catalog.
    pub fn new() -> Self {
        Self::default()
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
            .map_err(|error| AnalyticsError::Execution(error.to_string()))?;
        let rows = execute_compiled(&compiled, conn.as_mut()).await?;
        conn.commit()
            .await
            .map_err(|error| AnalyticsError::Execution(error.to_string()))?;
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
        .map_err(|error| AnalyticsError::Execution(error.to_string()))?;
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
        .map_err(|error| AnalyticsError::Execution(error.to_string()))
}
