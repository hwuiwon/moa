//! Generic analytics catalog, query validation, and execution entrypoints.

pub mod catalog;
pub mod clickhouse_exec;
pub mod compiler;
pub mod dialect;
pub mod error;
pub mod executor;
pub mod query;

pub use catalog::{analytics_catalog, find_dataset, find_field};
pub use clickhouse_exec::AnalyticsClickHouseClient;
pub use compiler::{AnalyticsBindValue, AnalyticsCompiler, CompiledAnalyticsQuery};
pub use dialect::AnalyticsBackend;
pub use error::{AnalyticsError, Result};
pub use executor::AnalyticsService;
pub use query::{ValidatedAnalyticsQuery, validate_query};
