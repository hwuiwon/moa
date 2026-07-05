//! Query validation for the generic analytics request model.

use chrono::{DateTime, Utc};
use moa_core::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCatalogResponse, AnalyticsCell, AnalyticsField,
    AnalyticsFieldKind, AnalyticsFieldRole, AnalyticsFilterOperator, AnalyticsQueryRequest,
};

use crate::catalog::find_dataset;
use crate::error::{AnalyticsError, Result};

/// Default number of rows returned when a query omits an explicit limit.
pub const DEFAULT_QUERY_LIMIT: u32 = 100;
/// Maximum number of rows returned by the analytics service.
pub const MAX_QUERY_LIMIT: u32 = 1_000;
/// Maximum grouping dimensions per request.
pub const MAX_DIMENSIONS: usize = 6;
/// Maximum measures per request.
pub const MAX_MEASURES: usize = 12;
/// Maximum filters per request.
pub const MAX_FILTERS: usize = 20;
/// Maximum supported timestamp `between` filter span.
pub const MAX_TIME_WINDOW_DAYS: i64 = 366;

/// Query request after catalog and limit validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAnalyticsQuery {
    /// Original request after validation.
    pub request: AnalyticsQueryRequest,
    /// Effective row limit.
    pub limit: u32,
    /// Effective tenant scope.
    pub tenant_id: TenantId,
}

/// Validates a generic analytics query against the supplied catalog.
pub fn validate_query(
    catalog: &AnalyticsCatalogResponse,
    request: AnalyticsQueryRequest,
) -> Result<ValidatedAnalyticsQuery> {
    let dataset =
        find_dataset(catalog, &request.dataset).ok_or_else(|| AnalyticsError::UnknownDataset {
            dataset: request.dataset.clone(),
        })?;
    let tenant_id = request
        .tenant_id
        .ok_or(AnalyticsError::MissingTenantScope)?;

    if request.dimensions.is_empty() && request.measures.is_empty() {
        return Err(AnalyticsError::EmptySelection {
            dataset: request.dataset.clone(),
        });
    }
    if request.dimensions.len() > MAX_DIMENSIONS {
        return Err(AnalyticsError::TooManyDimensions {
            count: request.dimensions.len(),
            max: MAX_DIMENSIONS,
        });
    }
    if request.measures.len() > MAX_MEASURES {
        return Err(AnalyticsError::TooManyMeasures {
            count: request.measures.len(),
            max: MAX_MEASURES,
        });
    }
    if request.filters.len() > MAX_FILTERS {
        return Err(AnalyticsError::TooManyFilters {
            count: request.filters.len(),
            max: MAX_FILTERS,
        });
    }

    let limit = request.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    if limit > MAX_QUERY_LIMIT {
        return Err(AnalyticsError::LimitTooLarge {
            limit,
            max: MAX_QUERY_LIMIT,
        });
    }

    for dimension in &request.dimensions {
        let field = require_field(dataset.id.as_str(), &dataset.fields, &dimension.field)?;
        require_role(field, AnalyticsFieldRole::Dimension)?;
    }

    for measure in &request.measures {
        match measure.field.as_deref() {
            Some(field_id) => {
                let field = require_field(dataset.id.as_str(), &dataset.fields, field_id)?;
                require_role(field, AnalyticsFieldRole::Measure)?;
                require_aggregation(field, measure.aggregation)?;
            }
            None if measure.aggregation == AnalyticsAggregation::Count => {}
            None => {
                return Err(AnalyticsError::MissingMeasureField {
                    aggregation: aggregation_name(measure.aggregation),
                });
            }
        }
    }

    for filter in &request.filters {
        let field = require_field(dataset.id.as_str(), &dataset.fields, &filter.field)?;
        require_filter_operator(field, filter.operator)?;
        if filter.value.is_none() && filter_requires_value(filter.operator) {
            return Err(AnalyticsError::MissingFilterValue {
                field: filter.field.clone(),
                operator: filter_operator_name(filter.operator),
            });
        }
        validate_tenant_filter(
            filter.field.as_str(),
            filter.operator,
            filter.value.as_ref(),
            tenant_id,
        )?;
        validate_time_window(field, filter.operator, filter.value.as_ref())?;
    }

    for order in &request.order_by {
        if !selected_field_or_alias(&request, &order.field) {
            return Err(AnalyticsError::UnknownOrderField {
                field: order.field.clone(),
            });
        }
    }

    Ok(ValidatedAnalyticsQuery {
        request,
        limit,
        tenant_id,
    })
}

fn require_field<'a>(
    dataset_id: &str,
    fields: &'a [AnalyticsField],
    field_id: &str,
) -> Result<&'a AnalyticsField> {
    fields
        .iter()
        .find(|field| field.id == field_id)
        .ok_or_else(|| AnalyticsError::UnknownField {
            dataset: dataset_id.to_string(),
            field: field_id.to_string(),
        })
}

fn require_role(field: &AnalyticsField, role: AnalyticsFieldRole) -> Result<()> {
    if field.role == role {
        return Ok(());
    }

    Err(AnalyticsError::UnsupportedFieldRole {
        field: field.id.clone(),
        role: role_name(role),
    })
}

fn require_aggregation(field: &AnalyticsField, aggregation: AnalyticsAggregation) -> Result<()> {
    if field.aggregations.contains(&aggregation) {
        return Ok(());
    }

    Err(AnalyticsError::UnsupportedAggregation {
        field: field.id.clone(),
        aggregation: aggregation_name(aggregation),
    })
}

fn require_filter_operator(
    field: &AnalyticsField,
    operator: AnalyticsFilterOperator,
) -> Result<()> {
    if field.filter_operators.contains(&operator) {
        return Ok(());
    }

    Err(AnalyticsError::UnsupportedFilterOperator {
        field: field.id.clone(),
        operator: filter_operator_name(operator),
    })
}

fn selected_field_or_alias(request: &AnalyticsQueryRequest, field: &str) -> bool {
    request
        .dimensions
        .iter()
        .any(|dimension| dimension.field == field || dimension.alias.as_deref() == Some(field))
        || request.measures.iter().any(|measure| {
            measure.field.as_deref() == Some(field)
                || measure.alias.as_deref() == Some(field)
                || measure
                    .field
                    .as_deref()
                    .map(|field_id| {
                        format!("{}_{}", aggregation_name(measure.aggregation), field_id)
                    })
                    .as_deref()
                    == Some(field)
                || (measure.field.is_none()
                    && measure.aggregation == AnalyticsAggregation::Count
                    && field == "count")
        })
}

fn filter_requires_value(operator: AnalyticsFilterOperator) -> bool {
    !matches!(
        operator,
        AnalyticsFilterOperator::IsNull | AnalyticsFilterOperator::IsNotNull
    )
}

fn validate_tenant_filter(
    field: &str,
    operator: AnalyticsFilterOperator,
    value: Option<&AnalyticsCell>,
    tenant_id: TenantId,
) -> Result<()> {
    if field != "tenant_id" {
        return Ok(());
    }
    if operator != AnalyticsFilterOperator::Eq {
        return Err(AnalyticsError::ConflictingTenantFilter);
    }
    let Some(value) = value.and_then(cell_as_string) else {
        return Err(AnalyticsError::ConflictingTenantFilter);
    };
    if value == tenant_id.to_string() {
        Ok(())
    } else {
        Err(AnalyticsError::ConflictingTenantFilter)
    }
}

fn validate_time_window(
    field: &AnalyticsField,
    operator: AnalyticsFilterOperator,
    value: Option<&AnalyticsCell>,
) -> Result<()> {
    if field.kind != AnalyticsFieldKind::Timestamp || operator != AnalyticsFilterOperator::Between {
        return Ok(());
    }
    let values = json_array(value).ok_or_else(|| AnalyticsError::InvalidFilterValue {
        field: field.id.clone(),
        reason: "between requires a two-element array".to_string(),
    })?;
    if values.len() != 2 {
        return Err(AnalyticsError::InvalidFilterValue {
            field: field.id.clone(),
            reason: "between requires exactly two values".to_string(),
        });
    }
    let start = parse_timestamp_json(&field.id, &values[0])?;
    let end = parse_timestamp_json(&field.id, &values[1])?;
    let days = end.signed_duration_since(start).num_days().abs();
    if days > MAX_TIME_WINDOW_DAYS {
        return Err(AnalyticsError::TimeWindowTooLarge {
            days,
            max_days: MAX_TIME_WINDOW_DAYS,
        });
    }
    Ok(())
}

fn parse_timestamp_json(field: &str, value: &serde_json::Value) -> Result<DateTime<Utc>> {
    let Some(value) = value.as_str() else {
        return Err(AnalyticsError::InvalidFilterValue {
            field: field.to_string(),
            reason: "timestamp value must be an RFC3339 string".to_string(),
        });
    };
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| AnalyticsError::InvalidFilterValue {
            field: field.to_string(),
            reason: error.to_string(),
        })
}

fn cell_as_string(cell: &AnalyticsCell) -> Option<&str> {
    match cell {
        AnalyticsCell::String(value) => Some(value.as_str()),
        _ => None,
    }
}

fn json_array(cell: Option<&AnalyticsCell>) -> Option<&[serde_json::Value]> {
    match cell {
        Some(AnalyticsCell::Json(serde_json::Value::Array(values))) => Some(values.as_slice()),
        _ => None,
    }
}

/// Returns the stable wire name for an analytics aggregation.
pub fn aggregation_name(aggregation: AnalyticsAggregation) -> &'static str {
    match aggregation {
        AnalyticsAggregation::Count => "count",
        AnalyticsAggregation::CountDistinct => "count_distinct",
        AnalyticsAggregation::Sum => "sum",
        AnalyticsAggregation::Avg => "avg",
        AnalyticsAggregation::Min => "min",
        AnalyticsAggregation::Max => "max",
        AnalyticsAggregation::P50 => "p50",
        AnalyticsAggregation::P95 => "p95",
        AnalyticsAggregation::P99 => "p99",
    }
}

/// Returns the stable wire name for an analytics filter operator.
pub fn filter_operator_name(operator: AnalyticsFilterOperator) -> &'static str {
    match operator {
        AnalyticsFilterOperator::Eq => "eq",
        AnalyticsFilterOperator::NotEq => "not_eq",
        AnalyticsFilterOperator::In => "in",
        AnalyticsFilterOperator::NotIn => "not_in",
        AnalyticsFilterOperator::Lt => "lt",
        AnalyticsFilterOperator::Lte => "lte",
        AnalyticsFilterOperator::Gt => "gt",
        AnalyticsFilterOperator::Gte => "gte",
        AnalyticsFilterOperator::Contains => "contains",
        AnalyticsFilterOperator::StartsWith => "starts_with",
        AnalyticsFilterOperator::EndsWith => "ends_with",
        AnalyticsFilterOperator::IsNull => "is_null",
        AnalyticsFilterOperator::IsNotNull => "is_not_null",
        AnalyticsFilterOperator::Between => "between",
    }
}

fn role_name(role: AnalyticsFieldRole) -> &'static str {
    match role {
        AnalyticsFieldRole::Dimension => "dimension",
        AnalyticsFieldRole::Measure => "measure",
        AnalyticsFieldRole::FilterOnly => "filter",
    }
}
