//! SQL compiler for validated analytics queries.

use moa_core::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCatalogResponse, AnalyticsCell, AnalyticsColumn,
    AnalyticsFieldKind, AnalyticsFieldRole, AnalyticsFilterOperator, AnalyticsQueryRequest,
    AnalyticsSortDirection,
};

use crate::catalog::{FieldSpec, analytics_catalog, find_dataset_spec, find_field_spec};
use crate::error::{AnalyticsError, Result};
use crate::query::{aggregation_name, validate_query};

/// Catalog-backed analytics query compiler.
#[derive(Debug, Clone)]
pub struct AnalyticsCompiler {
    catalog: AnalyticsCatalogResponse,
}

impl AnalyticsCompiler {
    /// Creates a compiler for the supplied catalog.
    pub fn new(catalog: AnalyticsCatalogResponse) -> Self {
        Self { catalog }
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
                dimension_select_expression(field)
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
                            measure_expression(measure.aggregation, field),
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
                        "COUNT(*)::BIGINT".to_string(),
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

        let mut bind_values = vec![AnalyticsBindValue::String(validated.tenant_id.to_string())];
        let mut predicates = vec!["d.tenant_id = $1::UUID".to_string()];
        for filter in &validated.request.filters {
            if filter.field == "tenant_id" {
                continue;
            }
            let field = required_spec(&dataset, &filter.field)?;
            predicates.push(filter_predicate(
                field,
                filter.operator,
                filter.value.as_ref(),
                &mut bind_values,
            )?);
        }

        let mut sql = format!(
            "SELECT {} FROM {} AS d WHERE {}",
            select_clauses.join(", "),
            dataset.relation_sql,
            predicates.join(" AND ")
        );
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

fn field_expression(field: &FieldSpec) -> String {
    format!("d.{}", field.column)
}

fn dimension_select_expression(field: &FieldSpec) -> String {
    match field.kind {
        AnalyticsFieldKind::Uuid => format!("{}::TEXT", field_expression(field)),
        _ => field_expression(field),
    }
}

fn measure_expression(aggregation: AnalyticsAggregation, field: &FieldSpec) -> String {
    let expression = field_expression(field);
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

fn filter_predicate(
    field: &FieldSpec,
    operator: AnalyticsFilterOperator,
    value: Option<&AnalyticsCell>,
    bind_values: &mut Vec<AnalyticsBindValue>,
) -> Result<String> {
    let expression = field_expression(field);
    match operator {
        AnalyticsFilterOperator::Eq => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} = {placeholder}"))
        }
        AnalyticsFilterOperator::NotEq => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} <> {placeholder}"))
        }
        AnalyticsFilterOperator::Lt => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} < {placeholder}"))
        }
        AnalyticsFilterOperator::Lte => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} <= {placeholder}"))
        }
        AnalyticsFilterOperator::Gt => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} > {placeholder}"))
        }
        AnalyticsFilterOperator::Gte => {
            let placeholder = push_cell_bind(field, value, bind_values)?;
            Ok(format!("{expression} >= {placeholder}"))
        }
        AnalyticsFilterOperator::Contains => {
            let placeholder = push_stringish_bind(field, value, bind_values)?;
            Ok(format!(
                "{expression}::TEXT ILIKE '%' || {placeholder} || '%'"
            ))
        }
        AnalyticsFilterOperator::StartsWith => {
            let placeholder = push_stringish_bind(field, value, bind_values)?;
            Ok(format!("{expression}::TEXT ILIKE {placeholder} || '%'"))
        }
        AnalyticsFilterOperator::EndsWith => {
            let placeholder = push_stringish_bind(field, value, bind_values)?;
            Ok(format!("{expression}::TEXT ILIKE '%' || {placeholder}"))
        }
        AnalyticsFilterOperator::In | AnalyticsFilterOperator::NotIn => {
            let placeholders = push_array_binds(field, value, bind_values)?;
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
            let placeholders = push_array_binds(field, value, bind_values)?;
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
    Ok(placeholder(field.kind, bind_values.len()))
}

fn push_stringish_bind(
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
    Ok(format!("${}", bind_values.len()))
}

fn push_array_binds(
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
        placeholders.push(placeholder(field.kind, bind_values.len()));
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

fn placeholder(kind: AnalyticsFieldKind, position: usize) -> String {
    match kind {
        AnalyticsFieldKind::Uuid => format!("${position}::UUID"),
        AnalyticsFieldKind::Timestamp => format!("${position}::TIMESTAMPTZ"),
        AnalyticsFieldKind::Integer => format!("${position}::BIGINT"),
        AnalyticsFieldKind::Float => format!("${position}::DOUBLE PRECISION"),
        AnalyticsFieldKind::Boolean => format!("${position}::BOOLEAN"),
        AnalyticsFieldKind::Json => format!("${position}::JSONB"),
        AnalyticsFieldKind::String => format!("${position}"),
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
            filters: Vec::new(),
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
        assert_eq!(compiled.bind_values.len(), 1);
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
