//! Query validation for the generic analytics request model.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCatalogResponse, AnalyticsCell, AnalyticsDataset,
    AnalyticsField, AnalyticsFieldKind, AnalyticsFieldRole, AnalyticsFilter,
    AnalyticsFilterOperator, AnalyticsQueryRequest,
};

use crate::catalog::find_dataset;
use crate::error::{Error, Result};

/// Default number of rows returned when a query omits an explicit limit.
pub(crate) const DEFAULT_QUERY_LIMIT: u32 = 100;
/// Maximum number of rows returned by the analytics service.
pub(crate) const MAX_QUERY_LIMIT: u32 = 1_000;
/// Maximum grouping dimensions per request.
pub(crate) const MAX_DIMENSIONS: usize = 6;
/// Maximum measures per request.
pub(crate) const MAX_MEASURES: usize = 12;
/// Maximum filters per request.
pub(crate) const MAX_FILTERS: usize = 20;
/// Maximum supported timestamp `between` filter span.
pub(crate) const MAX_TIME_WINDOW_DAYS: i64 = 366;

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
    validate_query_at(catalog, request, Utc::now())
}

/// Validates a query against a caller-supplied `now`, used by tests to pin the
/// time-window bound against a fixed clock.
pub(crate) fn validate_query_at(
    catalog: &AnalyticsCatalogResponse,
    request: AnalyticsQueryRequest,
    now: DateTime<Utc>,
) -> Result<ValidatedAnalyticsQuery> {
    let dataset = find_dataset(catalog, &request.dataset).ok_or_else(|| Error::UnknownDataset {
        dataset: request.dataset.clone(),
    })?;
    let tenant_id = request.tenant_id.ok_or(Error::MissingTenantScope)?;

    if request.dimensions.is_empty() && request.measures.is_empty() {
        return Err(Error::EmptySelection {
            dataset: request.dataset.clone(),
        });
    }
    if request.dimensions.len() > MAX_DIMENSIONS {
        return Err(Error::TooManyDimensions {
            count: request.dimensions.len(),
            max: MAX_DIMENSIONS,
        });
    }
    if request.measures.len() > MAX_MEASURES {
        return Err(Error::TooManyMeasures {
            count: request.measures.len(),
            max: MAX_MEASURES,
        });
    }
    if request.filters.len() > MAX_FILTERS {
        return Err(Error::TooManyFilters {
            count: request.filters.len(),
            max: MAX_FILTERS,
        });
    }

    let limit = request.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    if limit > MAX_QUERY_LIMIT {
        return Err(Error::LimitTooLarge {
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
                return Err(Error::MissingMeasureField {
                    aggregation: aggregation_name(measure.aggregation),
                });
            }
        }
    }

    for filter in &request.filters {
        let field = require_field(dataset.id.as_str(), &dataset.fields, &filter.field)?;
        require_filter_operator(field, filter.operator)?;
        if filter.value.is_none() && filter_requires_value(filter.operator) {
            return Err(Error::MissingFilterValue {
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

    enforce_required_time_window(dataset, &request.filters, now)?;

    for order in &request.order_by {
        if !selected_field_or_alias(&request, &order.field) {
            return Err(Error::UnknownOrderField {
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
        .ok_or_else(|| Error::UnknownField {
            dataset: dataset_id.to_string(),
            field: field_id.to_string(),
        })
}

fn require_role(field: &AnalyticsField, role: AnalyticsFieldRole) -> Result<()> {
    if field.role == role {
        return Ok(());
    }

    Err(Error::UnsupportedFieldRole {
        field: field.id.clone(),
        role: role_name(role),
    })
}

fn require_aggregation(field: &AnalyticsField, aggregation: AnalyticsAggregation) -> Result<()> {
    if field.aggregations.contains(&aggregation) {
        return Ok(());
    }

    Err(Error::UnsupportedAggregation {
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

    Err(Error::UnsupportedFilterOperator {
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
        return Err(Error::ConflictingTenantFilter);
    }
    let Some(value) = value.and_then(cell_as_string) else {
        return Err(Error::ConflictingTenantFilter);
    };
    if value == tenant_id.to_string() {
        Ok(())
    } else {
        Err(Error::ConflictingTenantFilter)
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
    let values = json_array(value).ok_or_else(|| Error::InvalidFilterValue {
        field: field.id.clone(),
        reason: "between requires a two-element array".to_string(),
    })?;
    if values.len() != 2 {
        return Err(Error::InvalidFilterValue {
            field: field.id.clone(),
            reason: "between requires exactly two values".to_string(),
        });
    }
    let start = parse_timestamp_json(&field.id, &values[0])?;
    let end = parse_timestamp_json(&field.id, &values[1])?;
    let days = end.signed_duration_since(start).num_days().abs();
    if days > MAX_TIME_WINDOW_DAYS {
        return Err(Error::TimeWindowTooLarge {
            days,
            max_days: MAX_TIME_WINDOW_DAYS,
        });
    }
    Ok(())
}

/// Requires a time-series dataset to carry a filter that bounds its scan to a
/// window no wider than [`MAX_TIME_WINDOW_DAYS`].
///
/// A dataset opts out of the requirement by declaring no `default_time_field`.
/// Otherwise at least one filter on that field must close the window: a
/// two-sided `between` (span already checked by [`validate_time_window`]), an
/// `eq` point, or a lower bound (`gte`/`gt`) whose start is no older than the
/// limit. An upper bound alone (`lt`/`lte`) leaves history unbounded and does
/// not satisfy the requirement.
fn enforce_required_time_window(
    dataset: &AnalyticsDataset,
    filters: &[AnalyticsFilter],
    now: DateTime<Utc>,
) -> Result<()> {
    let Some(time_field) = dataset.default_time_field.as_deref() else {
        return Ok(());
    };

    let mut bounded = false;
    for filter in filters {
        if filter.field != time_field {
            continue;
        }
        match filter.operator {
            AnalyticsFilterOperator::Between | AnalyticsFilterOperator::Eq => {
                bounded = true;
            }
            AnalyticsFilterOperator::Gte | AnalyticsFilterOperator::Gt => {
                let start = lower_bound_timestamp(time_field, filter.value.as_ref())?;
                let days = now.signed_duration_since(start).num_days();
                if days > MAX_TIME_WINDOW_DAYS {
                    return Err(Error::TimeWindowTooLarge {
                        days,
                        max_days: MAX_TIME_WINDOW_DAYS,
                    });
                }
                bounded = true;
            }
            _ => {}
        }
    }

    if bounded {
        Ok(())
    } else {
        Err(Error::MissingTimeWindow {
            dataset: dataset.id.clone(),
            time_field: time_field.to_string(),
            max_days: MAX_TIME_WINDOW_DAYS,
        })
    }
}

fn lower_bound_timestamp(field: &str, value: Option<&AnalyticsCell>) -> Result<DateTime<Utc>> {
    let raw = value
        .and_then(cell_as_string)
        .ok_or_else(|| Error::InvalidFilterValue {
            field: field.to_string(),
            reason: "timestamp lower bound must be an RFC3339 string".to_string(),
        })?;
    DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| Error::InvalidFilterValue {
            field: field.to_string(),
            reason: error.to_string(),
        })
}

fn parse_timestamp_json(field: &str, value: &serde_json::Value) -> Result<DateTime<Utc>> {
    let Some(value) = value.as_str() else {
        return Err(Error::InvalidFilterValue {
            field: field.to_string(),
            reason: "timestamp value must be an RFC3339 string".to_string(),
        });
    };
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| Error::InvalidFilterValue {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::analytics_catalog;
    use chrono::TimeZone;
    use moa_core::wire::analytics::{AnalyticsDimension, AnalyticsMeasure};

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 10, 12, 0, 0)
            .single()
            .expect("now")
    }

    fn sessions_request(filters: Vec<AnalyticsFilter>) -> AnalyticsQueryRequest {
        AnalyticsQueryRequest {
            dataset: "sessions".to_string(),
            tenant_id: Some(TenantId::new()),
            dimensions: vec![AnalyticsDimension {
                field: "channel".to_string(),
                alias: None,
            }],
            measures: vec![AnalyticsMeasure {
                field: None,
                aggregation: AnalyticsAggregation::Count,
                alias: None,
            }],
            filters,
            order_by: Vec::new(),
            limit: Some(10),
        }
    }

    fn created_at(op: AnalyticsFilterOperator, value: &str) -> AnalyticsFilter {
        AnalyticsFilter {
            field: "created_at".to_string(),
            operator: op,
            value: Some(AnalyticsCell::String(value.to_string())),
        }
    }

    #[test]
    fn rejects_query_without_time_window() {
        // Pins: a time-series dataset queried with no time filter is rejected so it
        // cannot scan a tenant's full event history.
        let error = validate_query_at(
            &analytics_catalog(),
            sessions_request(Vec::new()),
            fixed_now(),
        )
        .expect_err("unbounded query must be rejected");
        assert!(matches!(error, Error::MissingTimeWindow { .. }));
    }

    #[test]
    fn rejects_upper_bound_only_time_window() {
        // Pins: an upper bound alone (`lt`/`lte`) leaves history unbounded.
        let filters = vec![created_at(
            AnalyticsFilterOperator::Lt,
            "2026-07-01T00:00:00Z",
        )];
        let error = validate_query_at(&analytics_catalog(), sessions_request(filters), fixed_now())
            .expect_err("upper-bound-only must be rejected");
        assert!(matches!(error, Error::MissingTimeWindow { .. }));
    }

    #[test]
    fn rejects_lower_bound_older_than_max_window() {
        // Pins: a lower bound older than the limit is a too-wide window, not a pass.
        let filters = vec![created_at(
            AnalyticsFilterOperator::Gte,
            "2024-01-01T00:00:00Z",
        )];
        let error = validate_query_at(&analytics_catalog(), sessions_request(filters), fixed_now())
            .expect_err("too-wide lower bound must be rejected");
        assert!(matches!(error, Error::TimeWindowTooLarge { .. }));
    }

    #[test]
    fn accepts_recent_lower_bound() {
        let filters = vec![created_at(
            AnalyticsFilterOperator::Gte,
            "2026-06-10T00:00:00Z",
        )];
        validate_query_at(&analytics_catalog(), sessions_request(filters), fixed_now())
            .expect("a recent lower bound is accepted");
    }

    #[test]
    fn accepts_between_within_window() {
        let between = AnalyticsFilter {
            field: "created_at".to_string(),
            operator: AnalyticsFilterOperator::Between,
            value: Some(AnalyticsCell::Json(serde_json::json!([
                "2026-06-01T00:00:00Z",
                "2026-07-01T00:00:00Z"
            ]))),
        };
        validate_query_at(
            &analytics_catalog(),
            sessions_request(vec![between]),
            fixed_now(),
        )
        .expect("a bounded between is accepted");
    }

    #[test]
    fn accepts_eq_point_bound() {
        let filters = vec![created_at(
            AnalyticsFilterOperator::Eq,
            "2026-06-10T00:00:00Z",
        )];
        validate_query_at(&analytics_catalog(), sessions_request(filters), fixed_now())
            .expect("an eq point bound is accepted");
    }

    #[test]
    fn dataset_without_time_field_is_exempt() {
        // Pins: a dataset that declares no default_time_field opts out of the window
        // requirement instead of being unqueryable.
        let dataset = AnalyticsDataset {
            id: "catalog_meta".to_string(),
            label: "Meta".to_string(),
            description: String::new(),
            default_time_field: None,
            fields: Vec::new(),
        };
        enforce_required_time_window(&dataset, &[], fixed_now())
            .expect("a dataset without a time field is exempt");
    }
}
