//! Generic analytics catalog, query validation, and execution entrypoints.

pub mod catalog;
pub mod compiler;
pub mod error;
pub mod executor;
pub mod query;

pub use catalog::{analytics_catalog, find_dataset, find_field};
pub use compiler::{AnalyticsCompiler, CompiledAnalyticsQuery};
pub use error::{AnalyticsError, Result};
pub use executor::AnalyticsService;
pub use query::{ValidatedAnalyticsQuery, validate_query};
