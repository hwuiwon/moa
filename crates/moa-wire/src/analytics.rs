//! Analytics service wire DTOs.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};

/// Response payload describing the analytics datasets available to query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsCatalogResponse {
    /// Queryable analytics datasets.
    #[serde(default)]
    pub datasets: Vec<AnalyticsDataset>,
}

/// Catalog entry for one allowlisted analytics dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsDataset {
    /// Stable dataset identifier used by query requests.
    pub id: String,
    /// Human-readable dataset label.
    pub label: String,
    /// Human-readable dataset description.
    pub description: String,
    /// Default timestamp field for time-window filters, when the dataset has one.
    pub default_time_field: Option<String>,
    /// Fields that can be selected, aggregated, filtered, or ordered.
    #[serde(default)]
    pub fields: Vec<AnalyticsField>,
}

/// Catalog entry for one queryable field in an analytics dataset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsField {
    /// Stable field identifier used by query requests.
    pub id: String,
    /// Human-readable field label.
    pub label: String,
    /// Human-readable field description.
    pub description: String,
    /// Field data kind exposed to clients.
    pub kind: AnalyticsFieldKind,
    /// Field query role.
    pub role: AnalyticsFieldRole,
    /// Aggregations supported when this field is used as a measure.
    #[serde(default)]
    pub aggregations: Vec<AnalyticsAggregation>,
    /// Filter operators supported for this field.
    #[serde(default)]
    pub filter_operators: Vec<AnalyticsFilterOperator>,
}

/// Role a field plays in the analytics query model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsFieldRole {
    /// Field can be grouped or returned as a table column.
    Dimension,
    /// Field can be aggregated as a measure.
    Measure,
    /// Field can be filtered but is not selectable by default.
    FilterOnly,
}

/// Data kind exposed for a field or response column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsFieldKind {
    /// UTF-8 text value.
    String,
    /// Signed or unsigned integer value.
    Integer,
    /// Floating-point numeric value.
    Float,
    /// Boolean value.
    Boolean,
    /// RFC3339 timestamp value.
    Timestamp,
    /// UUID value serialized as a string.
    Uuid,
    /// Structured JSON value.
    Json,
}

/// Aggregation allowed for analytics measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsAggregation {
    /// Count matching rows.
    Count,
    /// Count distinct values of a field.
    CountDistinct,
    /// Sum numeric values.
    Sum,
    /// Average numeric values.
    Avg,
    /// Minimum value.
    Min,
    /// Maximum value.
    Max,
    /// Median percentile over numeric values.
    P50,
    /// Ninety-fifth percentile over numeric values.
    P95,
    /// Ninety-ninth percentile over numeric values.
    P99,
}

/// Filter operator allowed by the analytics query contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsFilterOperator {
    /// Field equals the supplied value.
    Eq,
    /// Field does not equal the supplied value.
    NotEq,
    /// Field is contained in the supplied array.
    In,
    /// Field is not contained in the supplied array.
    NotIn,
    /// Field is less than the supplied value.
    Lt,
    /// Field is less than or equal to the supplied value.
    Lte,
    /// Field is greater than the supplied value.
    Gt,
    /// Field is greater than or equal to the supplied value.
    Gte,
    /// Field contains the supplied text.
    Contains,
    /// Field starts with the supplied text.
    StartsWith,
    /// Field ends with the supplied text.
    EndsWith,
    /// Field is null.
    IsNull,
    /// Field is not null.
    IsNotNull,
    /// Field is between the supplied lower and upper values.
    Between,
}

/// Sort direction for analytics query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyticsSortDirection {
    /// Sort ascending.
    Asc,
    /// Sort descending.
    Desc,
}

/// Generic analytics query request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyticsQueryRequest {
    /// Dataset identifier from `AnalyticsCatalogResponse`.
    pub dataset: String,
    /// Optional tenant requested by the caller; edge authorization decides the effective tenant.
    pub tenant_id: Option<TenantId>,
    /// Grouping/table dimensions to return.
    #[serde(default)]
    pub dimensions: Vec<AnalyticsDimension>,
    /// Aggregated measures to return.
    #[serde(default)]
    pub measures: Vec<AnalyticsMeasure>,
    /// Filters applied before grouping.
    #[serde(default)]
    pub filters: Vec<AnalyticsFilter>,
    /// Result ordering.
    #[serde(default)]
    pub order_by: Vec<AnalyticsOrderBy>,
    /// Maximum number of rows to return.
    pub limit: Option<u32>,
}

/// Dimension selected by a generic analytics query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsDimension {
    /// Field identifier from the selected dataset.
    pub field: String,
    /// Optional response column identifier.
    pub alias: Option<String>,
}

/// Measure selected by a generic analytics query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsMeasure {
    /// Field identifier from the selected dataset, omitted for `count`.
    pub field: Option<String>,
    /// Aggregation to apply.
    pub aggregation: AnalyticsAggregation,
    /// Optional response column identifier.
    pub alias: Option<String>,
}

/// Filter applied by a generic analytics query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyticsFilter {
    /// Field identifier from the selected dataset.
    pub field: String,
    /// Filter operator to apply.
    pub operator: AnalyticsFilterOperator,
    /// JSON-compatible filter value. Null-only operators do not require a value.
    pub value: Option<AnalyticsCell>,
}

/// Ordering clause applied to generic analytics query results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsOrderBy {
    /// Field, dimension alias, or measure alias to order by.
    pub field: String,
    /// Sort direction.
    pub direction: AnalyticsSortDirection,
}

/// Generic analytics query response for aggregate and table-shaped results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyticsQueryResponse {
    /// Ordered column metadata matching each row cell position.
    #[serde(default)]
    pub columns: Vec<AnalyticsColumn>,
    /// Result rows with cells ordered to match `columns`.
    #[serde(default)]
    pub rows: Vec<Vec<AnalyticsCell>>,
    /// Query metadata calculated by the analytics service.
    pub metadata: AnalyticsQueryMetadata,
}

/// Response column metadata for one analytics result cell position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsColumn {
    /// Stable response column identifier.
    pub id: String,
    /// Human-readable response column label.
    pub label: String,
    /// Column value kind.
    pub kind: AnalyticsFieldKind,
    /// Column role in the response.
    pub role: AnalyticsFieldRole,
}

/// Metadata attached to a generic analytics query response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsQueryMetadata {
    /// Tenant used after edge authorization and service scoping.
    pub effective_tenant_id: Option<TenantId>,
    /// Dataset that was queried.
    pub dataset: String,
    /// Number of rows returned in this response.
    pub row_count: u64,
    /// Last known refresh timestamp for the backing read model, when available.
    pub read_model_updated_at: Option<DateTime<Utc>>,
}

/// JSON-compatible analytics result or filter value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnalyticsCell {
    /// JSON null.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number.
    Number(serde_json::Number),
    /// JSON string.
    String(String),
    /// JSON array or object.
    Json(serde_json::Value),
}
