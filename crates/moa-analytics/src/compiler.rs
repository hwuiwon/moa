//! SQL compiler for validated analytics queries.

use moa_core::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCatalogResponse, AnalyticsCell, AnalyticsColumn,
    AnalyticsFieldKind, AnalyticsFieldRole, AnalyticsFilterOperator, AnalyticsQueryRequest,
    AnalyticsSortDirection,
};

use crate::catalog::{FieldSpec, analytics_catalog, find_dataset_spec, find_field_spec};
use crate::dialect::{AnalyticsBackend, clickhouse_field_expr, clickhouse_from_sql};
use crate::error::{AnalyticsError, Result};
use crate::query::{aggregation_name, validate_query};

/// Catalog-backed analytics query compiler.
#[derive(Debug, Clone)]
pub struct AnalyticsCompiler {
    catalog: AnalyticsCatalogResponse,
    backend: AnalyticsBackend,
}

impl AnalyticsCompiler {
    /// Creates a Postgres-backed compiler for the supplied catalog.
    pub fn new(catalog: AnalyticsCatalogResponse) -> Self {
        Self::with_backend(catalog, AnalyticsBackend::Postgres)
    }

    /// Creates a compiler that emits SQL for the requested backend.
    pub fn with_backend(catalog: AnalyticsCatalogResponse, backend: AnalyticsBackend) -> Self {
        Self { catalog, backend }
    }

    /// Returns the backend this compiler emits SQL for.
    pub fn backend(&self) -> AnalyticsBackend {
        self.backend
    }

    /// Validates a request and compiles SQL plus response column metadata.
    pub fn compile(&self, request: AnalyticsQueryRequest) -> Result<CompiledAnalyticsQuery> {
        let validated = validate_query(&self.catalog, request)?;
        let dataset_id = validated.request.dataset.clone();
        let dataset =
            find_dataset_spec(&dataset_id).ok_or_else(|| AnalyticsError::UnknownDataset {
                dataset: dataset_id.clone(),
            })?;
        let mut columns = Vec::with_capacity(
            validated.request.dimensions.len() + validated.request.measures.len(),
        );
        let mut select_clauses = Vec::with_capacity(columns.capacity());
        let mut group_by_positions = Vec::with_capacity(validated.request.dimensions.len());
        let mut selected = Vec::with_capacity(columns.capacity());

        let backend = self.backend;
        for dimension in &validated.request.dimensions {
            let field = required_spec(&dataset, &dimension.field)?;
            let column_index = columns.len();
            let response_id = dimension
                .alias
                .clone()
                .unwrap_or_else(|| dimension.field.clone());
            let sql_alias = sql_alias(column_index);
            select_clauses.push(format!(
                "{} AS {sql_alias}",
                dimension_select_expression(backend, &dataset_id, field)?
            ));
            group_by_positions.push(column_index + 1);
            selected.push(SelectedOutput {
                response_id: response_id.clone(),
                source_field: dimension.field.clone(),
                sql_alias,
            });
            columns.push(AnalyticsColumn {
                id: response_id,
                label: field.label.to_string(),
                kind: field.kind,
                role: AnalyticsFieldRole::Dimension,
            });
        }

        for measure in &validated.request.measures {
            let column_index = columns.len();
            let sql_alias = sql_alias(column_index);
            let (response_id, label, kind, expression, source_field) =
                match measure.field.as_deref() {
                    Some(field_id) => {
                        let field = required_spec(&dataset, field_id)?;
                        let response_id = measure.alias.clone().unwrap_or_else(|| {
                            format!("{}_{}", aggregation_name(measure.aggregation), field.id)
                        });
                        (
                            response_id,
                            format!("{} {}", aggregation_name(measure.aggregation), field.label),
                            measure_kind(measure.aggregation, field.kind),
                            measure_expression(backend, &dataset_id, measure.aggregation, field)?,
                            Some(field_id.to_string()),
                        )
                    }
                    None => (
                        measure
                            .alias
                            .clone()
                            .unwrap_or_else(|| aggregation_name(measure.aggregation).to_string()),
                        "count".to_string(),
                        AnalyticsFieldKind::Integer,
                        count_star_expression(backend, &dataset_id),
                        None,
                    ),
                };
            select_clauses.push(format!("{expression} AS {sql_alias}"));
            selected.push(SelectedOutput {
                response_id: response_id.clone(),
                source_field: source_field.unwrap_or_else(|| response_id.clone()),
                sql_alias,
            });
            columns.push(AnalyticsColumn {
                id: response_id,
                label,
                kind,
                role: AnalyticsFieldRole::Measure,
            });
        }

        // The tenant id is always bind #1 in appearance order. For most datasets
        // the tenant predicate sits in the outer WHERE on the driving table; a
        // ClickHouse source may instead inject the filter into its own subquery
        // (marked with `$TENANT$`) so a dedup/`FINAL` runs behind the tenant
        // primary-key filter. Either way the value is bound first.
        let tenant_placeholder = placeholder(backend, AnalyticsFieldKind::Uuid, 1);
        let (from_sql, tenant_in_source) = match backend {
            AnalyticsBackend::Postgres => (format!("{} AS d", dataset.relation_sql), false),
            AnalyticsBackend::ClickHouse => {
                let raw = clickhouse_from_sql(&dataset_id).ok_or_else(|| {
                    AnalyticsError::BackendFieldUnavailable {
                        dataset: dataset_id.clone(),
                        field: "*".to_string(),
                        backend: backend.as_str(),
                    }
                })?;
                if raw.contains("$TENANT$") {
                    (raw.replace("$TENANT$", &tenant_placeholder), true)
                } else {
                    (raw.to_string(), false)
                }
            }
        };

        let mut bind_values = vec![AnalyticsBindValue::String(validated.tenant_id.to_string())];
        let mut predicates = Vec::new();
        if !tenant_in_source {
            predicates.push(format!("d.tenant_id = {tenant_placeholder}"));
        }
        for filter in &validated.request.filters {
            if filter.field == "tenant_id" {
                continue;
            }
            let field = required_spec(&dataset, &filter.field)?;
            predicates.push(filter_predicate(
                backend,
                &dataset_id,
                field,
                filter.operator,
                filter.value.as_ref(),
                &mut bind_values,
            )?);
        }

        let mut sql = format!("SELECT {} FROM {}", select_clauses.join(", "), from_sql);
        if !predicates.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&predicates.join(" AND "));
        }
        if !group_by_positions.is_empty() {
            let positions = group_by_positions
                .into_iter()
                .map(|position| position.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(" GROUP BY ");
            sql.push_str(&positions);
        }
        if !validated.request.order_by.is_empty() {
            let order = validated
                .request
                .order_by
                .iter()
                .map(|order| {
                    let selected = selected.iter().find(|selected| {
                        selected.response_id == order.field || selected.source_field == order.field
                    });
                    let Some(selected) = selected else {
                        return Err(AnalyticsError::UnknownOrderField {
                            field: order.field.clone(),
                        });
                    };
                    Ok(format!(
                        "{} {}",
                        selected.sql_alias,
                        match order.direction {
                            AnalyticsSortDirection::Asc => "ASC",
                            AnalyticsSortDirection::Desc => "DESC",
                        }
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            sql.push_str(" ORDER BY ");
            sql.push_str(&order.join(", "));
        }
        sql.push_str(" LIMIT ");
        sql.push_str(&validated.limit.to_string());

        Ok(CompiledAnalyticsQuery {
            dataset: dataset_id,
            columns,
            limit: validated.limit,
            effective_tenant_id: validated.tenant_id,
            sql,
            bind_values,
        })
    }
}

impl Default for AnalyticsCompiler {
    fn default() -> Self {
        Self::new(analytics_catalog())
    }
}

/// Compiled analytics query plan.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledAnalyticsQuery {
    /// Dataset selected by the query.
    pub dataset: String,
    /// Ordered response columns expected from execution.
    pub columns: Vec<AnalyticsColumn>,
    /// Effective row limit.
    pub limit: u32,
    /// Effective tenant enforced by the compiler.
    pub effective_tenant_id: TenantId,
    /// SQL statement generated from allowlisted catalog metadata.
    pub sql: String,
    /// Ordered SQL bind values.
    pub bind_values: Vec<AnalyticsBindValue>,
}

/// SQL bind values produced by the analytics compiler.
#[derive(Debug, Clone, PartialEq)]
pub enum AnalyticsBindValue {
    /// Text, UUID, or timestamp value bound as text and cast by SQL when needed.
    String(String),
    /// Signed integer value.
    Integer(i64),
    /// Floating-point value.
    Float(f64),
    /// Boolean value.
    Bool(bool),
    /// JSON value.
    Json(serde_json::Value),
}

#[derive(Debug)]
struct SelectedOutput {
    response_id: String,
    source_field: String,
    sql_alias: String,
}

fn required_spec<'a>(
    dataset: &'a crate::catalog::DatasetSpec,
    field_id: &str,
) -> Result<&'a FieldSpec> {
    find_field_spec(dataset, field_id).ok_or_else(|| AnalyticsError::UnknownField {
        dataset: dataset.id.to_string(),
        field: field_id.to_string(),
    })
}

fn sql_alias(index: usize) -> String {
    format!("c{index}")
}

fn field_expression(
    backend: AnalyticsBackend,
    dataset_id: &str,
    field: &FieldSpec,
) -> Result<String> {
    match backend {
        AnalyticsBackend::Postgres => Ok(format!("d.{}", field.column)),
        AnalyticsBackend::ClickHouse => clickhouse_field_expr(dataset_id, field.id)
            .map(str::to_string)
            .ok_or_else(|| AnalyticsError::BackendFieldUnavailable {
                dataset: dataset_id.to_string(),
                field: field.id.to_string(),
                backend: backend.as_str(),
            }),
    }
}

fn dimension_select_expression(
    backend: AnalyticsBackend,
    dataset_id: &str,
    field: &FieldSpec,
) -> Result<String> {
    let expression = field_expression(backend, dataset_id, field)?;
    Ok(match (backend, field.kind) {
        (AnalyticsBackend::Postgres, AnalyticsFieldKind::Uuid) => format!("{expression}::TEXT"),
        (AnalyticsBackend::ClickHouse, AnalyticsFieldKind::Uuid) => {
            format!("toString({expression})")
        }
        // ClickHouse timestamps are emitted as microsecond epochs so the executor
        // can decode a stable integer regardless of the server datetime format.
        (AnalyticsBackend::ClickHouse, AnalyticsFieldKind::Timestamp) => {
            format!("toUnixTimestamp64Micro({expression})")
        }
        _ => expression,
    })
}

fn count_star_expression(backend: AnalyticsBackend, dataset_id: &str) -> String {
    match backend {
        AnalyticsBackend::Postgres => "COUNT(*)::BIGINT".to_string(),
        // Counts over the un-`FINAL` events_raw stream must be duplicate-tolerant.
        // The events source is deduped to one row per (session_id, sequence_num),
        // so uniqExact over the event id is an exact row count that also tolerates
        // any duplicate that slips past the dedup.
        AnalyticsBackend::ClickHouse if dataset_id == "events" => {
            "uniqExact(d.event_id)".to_string()
        }
        AnalyticsBackend::ClickHouse => "count()".to_string(),
    }
}

fn measure_expression(
    backend: AnalyticsBackend,
    dataset_id: &str,
    aggregation: AnalyticsAggregation,
    field: &FieldSpec,
) -> Result<String> {
    let expression = field_expression(backend, dataset_id, field)?;
    Ok(match backend {
        AnalyticsBackend::Postgres => postgres_measure_expression(aggregation, field, &expression),
        AnalyticsBackend::ClickHouse => {
            clickhouse_measure_expression(aggregation, field, &expression)
        }
    })
}

fn postgres_measure_expression(
    aggregation: AnalyticsAggregation,
    field: &FieldSpec,
    expression: &str,
) -> String {
    match aggregation {
        AnalyticsAggregation::Count => format!("COUNT({expression})::BIGINT"),
        AnalyticsAggregation::CountDistinct => format!("COUNT(DISTINCT {expression})::BIGINT"),
        AnalyticsAggregation::Sum => match field.kind {
            AnalyticsFieldKind::Integer => format!("COALESCE(SUM({expression}), 0)::BIGINT"),
            AnalyticsFieldKind::Float => {
                format!("COALESCE(SUM({expression}), 0.0)::DOUBLE PRECISION")
            }
            _ => format!("COUNT({expression})::BIGINT"),
        },
        AnalyticsAggregation::Avg => format!("AVG({expression})::DOUBLE PRECISION"),
        AnalyticsAggregation::Min => match field.kind {
            AnalyticsFieldKind::Uuid => format!("MIN({expression}::TEXT)"),
            _ => format!("MIN({expression})"),
        },
        AnalyticsAggregation::Max => match field.kind {
            AnalyticsFieldKind::Uuid => format!("MAX({expression}::TEXT)"),
            _ => format!("MAX({expression})"),
        },
        AnalyticsAggregation::P50 => {
            format!("PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY {expression})::DOUBLE PRECISION")
        }
        AnalyticsAggregation::P95 => {
            format!("PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY {expression})::DOUBLE PRECISION")
        }
        AnalyticsAggregation::P99 => {
            format!("PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY {expression})::DOUBLE PRECISION")
        }
    }
}

/// Emits the ClickHouse aggregate for a measure.
///
/// `quantileExactInclusive(p)(x)` is ClickHouse's exact, linearly interpolated
/// percentile with inclusive rank — the same definition as Postgres
/// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x)` — so the two dialects return
/// identical percentile cells for the same data.
fn clickhouse_measure_expression(
    aggregation: AnalyticsAggregation,
    field: &FieldSpec,
    expression: &str,
) -> String {
    match aggregation {
        AnalyticsAggregation::Count => format!("count({expression})"),
        AnalyticsAggregation::CountDistinct => format!("uniqExact({expression})"),
        AnalyticsAggregation::Sum => match field.kind {
            AnalyticsFieldKind::Integer => format!("ifNull(sum({expression}), 0)"),
            AnalyticsFieldKind::Float => format!("ifNull(sum({expression}), 0.0)"),
            _ => format!("count({expression})"),
        },
        AnalyticsAggregation::Avg => format!("avg({expression})"),
        AnalyticsAggregation::Min => format!("min({expression})"),
        AnalyticsAggregation::Max => format!("max({expression})"),
        AnalyticsAggregation::P50 => format!("quantileExactInclusive(0.5)({expression})"),
        AnalyticsAggregation::P95 => format!("quantileExactInclusive(0.95)({expression})"),
        AnalyticsAggregation::P99 => format!("quantileExactInclusive(0.99)({expression})"),
    }
}

fn filter_predicate(
    backend: AnalyticsBackend,
    dataset_id: &str,
    field: &FieldSpec,
    operator: AnalyticsFilterOperator,
    value: Option<&AnalyticsCell>,
    bind_values: &mut Vec<AnalyticsBindValue>,
) -> Result<String> {
    let expression = field_expression(backend, dataset_id, field)?;
    match operator {
        AnalyticsFilterOperator::Eq => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} = {placeholder}"))
        }
        AnalyticsFilterOperator::NotEq => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} <> {placeholder}"))
        }
        AnalyticsFilterOperator::Lt => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} < {placeholder}"))
        }
        AnalyticsFilterOperator::Lte => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} <= {placeholder}"))
        }
        AnalyticsFilterOperator::Gt => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} > {placeholder}"))
        }
        AnalyticsFilterOperator::Gte => {
            let placeholder = push_cell_bind(backend, field, value, bind_values)?;
            Ok(format!("{expression} >= {placeholder}"))
        }
        AnalyticsFilterOperator::Contains => {
            let placeholder = push_stringish_bind(backend, field, value, bind_values)?;
            Ok(match backend {
                AnalyticsBackend::Postgres => {
                    format!("{expression}::TEXT ILIKE '%' || {placeholder} || '%'")
                }
                AnalyticsBackend::ClickHouse => {
                    format!("positionCaseInsensitive(toString({expression}), {placeholder}) > 0")
                }
            })
        }
        AnalyticsFilterOperator::StartsWith => {
            let placeholder = push_stringish_bind(backend, field, value, bind_values)?;
            Ok(match backend {
                AnalyticsBackend::Postgres => {
                    format!("{expression}::TEXT ILIKE {placeholder} || '%'")
                }
                AnalyticsBackend::ClickHouse => {
                    format!("startsWith(lower(toString({expression})), lower({placeholder}))")
                }
            })
        }
        AnalyticsFilterOperator::EndsWith => {
            let placeholder = push_stringish_bind(backend, field, value, bind_values)?;
            Ok(match backend {
                AnalyticsBackend::Postgres => {
                    format!("{expression}::TEXT ILIKE '%' || {placeholder}")
                }
                AnalyticsBackend::ClickHouse => {
                    format!("endsWith(lower(toString({expression})), lower({placeholder}))")
                }
            })
        }
        AnalyticsFilterOperator::In | AnalyticsFilterOperator::NotIn => {
            let placeholders = push_array_binds(backend, field, value, bind_values)?;
            let keyword = if operator == AnalyticsFilterOperator::In {
                "IN"
            } else {
                "NOT IN"
            };
            Ok(format!(
                "{expression} {keyword} ({})",
                placeholders.join(", ")
            ))
        }
        AnalyticsFilterOperator::IsNull => Ok(format!("{expression} IS NULL")),
        AnalyticsFilterOperator::IsNotNull => Ok(format!("{expression} IS NOT NULL")),
        AnalyticsFilterOperator::Between => {
            let placeholders = push_array_binds(backend, field, value, bind_values)?;
            if placeholders.len() != 2 {
                return Err(AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "between requires exactly two values".to_string(),
                });
            }
            Ok(format!(
                "{expression} BETWEEN {} AND {}",
                placeholders[0], placeholders[1]
            ))
        }
    }
}

fn push_cell_bind(
    backend: AnalyticsBackend,
    field: &FieldSpec,
    value: Option<&AnalyticsCell>,
    bind_values: &mut Vec<AnalyticsBindValue>,
) -> Result<String> {
    let value = value.ok_or_else(|| AnalyticsError::MissingFilterValue {
        field: field.id.to_string(),
        operator: "filter",
    })?;
    let bind = bind_from_cell(field, value)?;
    bind_values.push(bind);
    Ok(placeholder(backend, field.kind, bind_values.len()))
}

fn push_stringish_bind(
    backend: AnalyticsBackend,
    field: &FieldSpec,
    value: Option<&AnalyticsCell>,
    bind_values: &mut Vec<AnalyticsBindValue>,
) -> Result<String> {
    let value = value.ok_or_else(|| AnalyticsError::MissingFilterValue {
        field: field.id.to_string(),
        operator: "filter",
    })?;
    let Some(value) = cell_string(value) else {
        return Err(AnalyticsError::InvalidFilterValue {
            field: field.id.to_string(),
            reason: "text filter requires a string value".to_string(),
        });
    };
    bind_values.push(AnalyticsBindValue::String(value.to_string()));
    Ok(text_placeholder(backend, bind_values.len()))
}

fn push_array_binds(
    backend: AnalyticsBackend,
    field: &FieldSpec,
    value: Option<&AnalyticsCell>,
    bind_values: &mut Vec<AnalyticsBindValue>,
) -> Result<Vec<String>> {
    let Some(AnalyticsCell::Json(serde_json::Value::Array(values))) = value else {
        return Err(AnalyticsError::InvalidFilterValue {
            field: field.id.to_string(),
            reason: "operator requires an array value".to_string(),
        });
    };
    if values.is_empty() {
        return Err(AnalyticsError::InvalidFilterValue {
            field: field.id.to_string(),
            reason: "array value must not be empty".to_string(),
        });
    }
    let mut placeholders = Vec::with_capacity(values.len());
    for value in values {
        bind_values.push(bind_from_json(field, value)?);
        placeholders.push(placeholder(backend, field.kind, bind_values.len()));
    }
    Ok(placeholders)
}

fn bind_from_cell(field: &FieldSpec, value: &AnalyticsCell) -> Result<AnalyticsBindValue> {
    match (field.kind, value) {
        (_, AnalyticsCell::Null) => Err(AnalyticsError::InvalidFilterValue {
            field: field.id.to_string(),
            reason: "null requires is_null or is_not_null".to_string(),
        }),
        (
            AnalyticsFieldKind::String | AnalyticsFieldKind::Uuid | AnalyticsFieldKind::Timestamp,
            _,
        ) => {
            let Some(value) = cell_string(value) else {
                return Err(AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "value must be a string".to_string(),
                });
            };
            Ok(AnalyticsBindValue::String(value.to_string()))
        }
        (AnalyticsFieldKind::Integer, AnalyticsCell::Number(number)) => number
            .as_i64()
            .map(AnalyticsBindValue::Integer)
            .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                field: field.id.to_string(),
                reason: "value must be an integer".to_string(),
            }),
        (AnalyticsFieldKind::Float, AnalyticsCell::Number(number)) => number
            .as_f64()
            .map(AnalyticsBindValue::Float)
            .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                field: field.id.to_string(),
                reason: "value must be numeric".to_string(),
            }),
        (AnalyticsFieldKind::Boolean, AnalyticsCell::Bool(value)) => {
            Ok(AnalyticsBindValue::Bool(*value))
        }
        (AnalyticsFieldKind::Json, AnalyticsCell::Json(value)) => {
            Ok(AnalyticsBindValue::Json(value.clone()))
        }
        _ => Err(AnalyticsError::InvalidFilterValue {
            field: field.id.to_string(),
            reason: "value kind does not match field kind".to_string(),
        }),
    }
}

fn bind_from_json(field: &FieldSpec, value: &serde_json::Value) -> Result<AnalyticsBindValue> {
    match field.kind {
        AnalyticsFieldKind::String | AnalyticsFieldKind::Uuid | AnalyticsFieldKind::Timestamp => {
            value
                .as_str()
                .map(|value| AnalyticsBindValue::String(value.to_string()))
                .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "array values must be strings".to_string(),
                })
        }
        AnalyticsFieldKind::Integer => {
            value
                .as_i64()
                .map(AnalyticsBindValue::Integer)
                .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "array values must be integers".to_string(),
                })
        }
        AnalyticsFieldKind::Float => {
            value
                .as_f64()
                .map(AnalyticsBindValue::Float)
                .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "array values must be numeric".to_string(),
                })
        }
        AnalyticsFieldKind::Boolean => {
            value
                .as_bool()
                .map(AnalyticsBindValue::Bool)
                .ok_or_else(|| AnalyticsError::InvalidFilterValue {
                    field: field.id.to_string(),
                    reason: "array values must be booleans".to_string(),
                })
        }
        AnalyticsFieldKind::Json => Ok(AnalyticsBindValue::Json(value.clone())),
    }
}

fn cell_string(value: &AnalyticsCell) -> Option<&str> {
    match value {
        AnalyticsCell::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn placeholder(backend: AnalyticsBackend, kind: AnalyticsFieldKind, position: usize) -> String {
    match backend {
        AnalyticsBackend::Postgres => match kind {
            AnalyticsFieldKind::Uuid => format!("${position}::UUID"),
            AnalyticsFieldKind::Timestamp => format!("${position}::TIMESTAMPTZ"),
            AnalyticsFieldKind::Integer => format!("${position}::BIGINT"),
            AnalyticsFieldKind::Float => format!("${position}::DOUBLE PRECISION"),
            AnalyticsFieldKind::Boolean => format!("${position}::BOOLEAN"),
            AnalyticsFieldKind::Json => format!("${position}::JSONB"),
            AnalyticsFieldKind::String => format!("${position}"),
        },
        // ClickHouse uses positional `?` binds; the bound value is text, so UUID
        // and timestamp comparisons wrap the placeholder in the parsing function
        // that yields the column's native type.
        AnalyticsBackend::ClickHouse => match kind {
            AnalyticsFieldKind::Uuid => "toUUID(?)".to_string(),
            AnalyticsFieldKind::Timestamp => "parseDateTime64BestEffort(?, 6, 'UTC')".to_string(),
            _ => "?".to_string(),
        },
    }
}

fn text_placeholder(backend: AnalyticsBackend, position: usize) -> String {
    match backend {
        AnalyticsBackend::Postgres => format!("${position}"),
        AnalyticsBackend::ClickHouse => "?".to_string(),
    }
}

fn measure_kind(
    aggregation: AnalyticsAggregation,
    field_kind: AnalyticsFieldKind,
) -> AnalyticsFieldKind {
    match aggregation {
        AnalyticsAggregation::Count | AnalyticsAggregation::CountDistinct => {
            AnalyticsFieldKind::Integer
        }
        AnalyticsAggregation::Avg
        | AnalyticsAggregation::P50
        | AnalyticsAggregation::P95
        | AnalyticsAggregation::P99 => AnalyticsFieldKind::Float,
        AnalyticsAggregation::Sum => match field_kind {
            AnalyticsFieldKind::Integer => AnalyticsFieldKind::Integer,
            _ => AnalyticsFieldKind::Float,
        },
        AnalyticsAggregation::Min | AnalyticsAggregation::Max => field_kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::wire::analytics::{
        AnalyticsDimension, AnalyticsFilter, AnalyticsMeasure, AnalyticsOrderBy,
    };

    fn request() -> AnalyticsQueryRequest {
        AnalyticsQueryRequest {
            dataset: "turns".to_string(),
            tenant_id: Some(TenantId::new()),
            dimensions: vec![AnalyticsDimension {
                field: "model".to_string(),
                alias: None,
            }],
            measures: vec![AnalyticsMeasure {
                field: Some("cost_cents".to_string()),
                aggregation: AnalyticsAggregation::P95,
                alias: Some("p95_cost".to_string()),
            }],
            // Time-series datasets require a bounded window; a recent lower bound
            // keeps the shared fixture valid against the wall clock.
            filters: vec![AnalyticsFilter {
                field: "finished_at".to_string(),
                operator: AnalyticsFilterOperator::Gte,
                value: Some(AnalyticsCell::String(
                    (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
                )),
            }],
            order_by: vec![AnalyticsOrderBy {
                field: "p95_cost".to_string(),
                direction: AnalyticsSortDirection::Desc,
            }],
            limit: Some(25),
        }
    }

    #[test]
    fn compiler_injects_tenant_scope_and_percentile_offline() {
        let compiled = AnalyticsCompiler::default()
            .compile(request())
            .expect("compile query");

        assert!(compiled.sql.contains("FROM analytics.turn_fact AS d"));
        assert!(compiled.sql.contains("d.tenant_id = $1::UUID"));
        assert!(compiled.sql.contains("PERCENTILE_CONT(0.95)"));
        assert!(compiled.sql.contains("ORDER BY c1 DESC"));
        // Tenant scope bind plus the required time-window lower bound.
        assert_eq!(compiled.bind_values.len(), 2);
        assert_eq!(compiled.limit, 25);
    }

    #[test]
    fn compiler_rejects_raw_field_fragments_offline() {
        let mut request = request();
        request.dimensions[0].field = "model; DROP TABLE sessions".to_string();

        let error = AnalyticsCompiler::default()
            .compile(request)
            .expect_err("unknown field should fail");
        assert!(matches!(error, AnalyticsError::UnknownField { .. }));
    }

    #[test]
    fn compiler_rejects_conflicting_tenant_filter_offline() {
        let mut request = request();
        request.filters.push(AnalyticsFilter {
            field: "tenant_id".to_string(),
            operator: AnalyticsFilterOperator::Eq,
            value: Some(AnalyticsCell::String(TenantId::new().to_string())),
        });

        let error = AnalyticsCompiler::default()
            .compile(request)
            .expect_err("tenant override should fail");
        assert!(matches!(error, AnalyticsError::ConflictingTenantFilter));
    }
}
