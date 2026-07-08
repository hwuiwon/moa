//! Error types for generic analytics catalog and query handling.

/// Result alias for analytics operations.
pub type Result<T> = std::result::Result<T, AnalyticsError>;

/// Errors returned by the analytics catalog, compiler, and executor.
#[derive(Debug, thiserror::Error)]
pub enum AnalyticsError {
    /// A query referenced a dataset that is not in the catalog.
    #[error("unknown analytics dataset `{dataset}`")]
    UnknownDataset {
        /// Requested dataset identifier.
        dataset: String,
    },
    /// A query referenced a field that is not in the selected dataset.
    #[error("unknown analytics field `{field}` for dataset `{dataset}`")]
    UnknownField {
        /// Selected dataset identifier.
        dataset: String,
        /// Requested field identifier.
        field: String,
    },
    /// A query selected a field using a role that is not allowed.
    #[error("analytics field `{field}` cannot be used as a {role}")]
    UnsupportedFieldRole {
        /// Requested field identifier.
        field: String,
        /// Requested role.
        role: &'static str,
    },
    /// A query used an aggregation that is not allowed for the field.
    #[error("aggregation `{aggregation}` is not allowed for field `{field}`")]
    UnsupportedAggregation {
        /// Requested field identifier.
        field: String,
        /// Requested aggregation identifier.
        aggregation: &'static str,
    },
    /// A query used a filter operator that is not allowed for the field.
    #[error("operator `{operator}` is not allowed for field `{field}`")]
    UnsupportedFilterOperator {
        /// Requested field identifier.
        field: String,
        /// Requested filter operator identifier.
        operator: &'static str,
    },
    /// A query supplied a malformed filter value.
    #[error("invalid filter value for field `{field}`: {reason}")]
    InvalidFilterValue {
        /// Requested field identifier.
        field: String,
        /// Human-readable validation failure.
        reason: String,
    },
    /// A measure omitted its field for an aggregation that requires one.
    #[error("aggregation `{aggregation}` requires a field")]
    MissingMeasureField {
        /// Requested aggregation identifier.
        aggregation: &'static str,
    },
    /// A filter omitted a value for an operator that requires one.
    #[error("operator `{operator}` on field `{field}` requires a value")]
    MissingFilterValue {
        /// Requested field identifier.
        field: String,
        /// Requested filter operator identifier.
        operator: &'static str,
    },
    /// A tenant-scoped analytics query did not carry an effective tenant.
    #[error("analytics query requires an effective tenant scope")]
    MissingTenantScope,
    /// A query tried to override the effective tenant scope.
    #[error("tenant filter conflicts with the effective tenant scope")]
    ConflictingTenantFilter,
    /// A query did not request any dimensions or measures.
    #[error(
        "analytics query for dataset `{dataset}` must select at least one dimension or measure"
    )]
    EmptySelection {
        /// Selected dataset identifier.
        dataset: String,
    },
    /// A query requested too many dimensions.
    #[error("analytics query requested {count} dimensions, maximum is {max}")]
    TooManyDimensions {
        /// Requested dimension count.
        count: usize,
        /// Maximum allowed dimension count.
        max: usize,
    },
    /// A query requested too many measures.
    #[error("analytics query requested {count} measures, maximum is {max}")]
    TooManyMeasures {
        /// Requested measure count.
        count: usize,
        /// Maximum allowed measure count.
        max: usize,
    },
    /// A query requested too many filters.
    #[error("analytics query requested {count} filters, maximum is {max}")]
    TooManyFilters {
        /// Requested filter count.
        count: usize,
        /// Maximum allowed filter count.
        max: usize,
    },
    /// A query requested more rows than the current service limit.
    #[error("analytics query limit {limit} exceeds maximum {max}")]
    LimitTooLarge {
        /// Requested row limit.
        limit: u32,
        /// Maximum allowed row limit.
        max: u32,
    },
    /// A query requested a time window wider than the service limit.
    #[error("analytics time window {days} days exceeds maximum {max_days}")]
    TimeWindowTooLarge {
        /// Requested time window in whole days.
        days: i64,
        /// Maximum allowed time window in whole days.
        max_days: i64,
    },
    /// A query tried to order by a field or alias that is not selected.
    #[error("order field `{field}` is not selected by the query")]
    UnknownOrderField {
        /// Requested order field or alias.
        field: String,
    },
    /// SQL execution failed.
    #[error("analytics query execution failed: {0}")]
    Execution(String),
    /// ClickHouse query execution or result decoding failed.
    #[error("analytics clickhouse query failed: {0}")]
    ClickHouse(String),
    /// A selected field has no expression mapping for the target backend.
    #[error("field `{field}` on dataset `{dataset}` is not available on the {backend} backend")]
    BackendFieldUnavailable {
        /// Selected dataset identifier.
        dataset: String,
        /// Requested field identifier.
        field: String,
        /// Target backend name.
        backend: &'static str,
    },
}
