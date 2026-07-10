//! Per-query budgets for the analytics serving path.
//!
//! Analytics validation caps the returned rows and dimensions, but without a
//! wall-clock budget an interactive dashboard query (for example an exact
//! ordered percentile) can scan and sort a tenant's full event history. These
//! knobs bound the database work each query is allowed to perform on both the
//! Postgres materialized-view backend and the ClickHouse read models.

use serde::{Deserialize, Serialize};

use crate::{MoaError, Result};

/// Per-query budgets applied by the analytics executors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalyticsConfig {
    /// Postgres `statement_timeout` applied to each analytics query transaction,
    /// in milliseconds. A runaway scan is cancelled server-side instead of
    /// holding a connection open indefinitely.
    pub statement_timeout_ms: u64,
    /// ClickHouse `max_execution_time` applied to each analytics query, in
    /// seconds.
    pub clickhouse_max_execution_time_secs: u64,
    /// ClickHouse `max_rows_to_read` applied to each analytics query.
    pub clickhouse_max_rows_to_read: u64,
    /// ClickHouse `max_bytes_to_read` applied to each analytics query.
    pub clickhouse_max_bytes_to_read: u64,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            statement_timeout_ms: 10_000,
            clickhouse_max_execution_time_secs: 10,
            clickhouse_max_rows_to_read: 1_000_000_000,
            clickhouse_max_bytes_to_read: 10_000_000_000,
        }
    }
}

impl AnalyticsConfig {
    /// Validates that every budget is a positive, enforceable value.
    pub fn validate(&self) -> Result<()> {
        if self.statement_timeout_ms == 0 {
            return Err(MoaError::ConfigError(
                "analytics.statement_timeout_ms must be greater than zero".to_string(),
            ));
        }
        if self.clickhouse_max_execution_time_secs == 0 {
            return Err(MoaError::ConfigError(
                "analytics.clickhouse_max_execution_time_secs must be greater than zero"
                    .to_string(),
            ));
        }
        if self.clickhouse_max_rows_to_read == 0 {
            return Err(MoaError::ConfigError(
                "analytics.clickhouse_max_rows_to_read must be greater than zero".to_string(),
            ));
        }
        if self.clickhouse_max_bytes_to_read == 0 {
            return Err(MoaError::ConfigError(
                "analytics.clickhouse_max_bytes_to_read must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budgets_are_positive_and_valid() {
        // Pins: the default analytics budgets are enforceable (all non-zero).
        let config = AnalyticsConfig::default();
        config.validate().expect("default budgets validate");
        assert_eq!(config.statement_timeout_ms, 10_000);
    }

    #[test]
    fn rejects_zero_budgets() {
        // Pins: a zero budget disables enforcement and is rejected at startup.
        for config in [
            AnalyticsConfig {
                statement_timeout_ms: 0,
                ..AnalyticsConfig::default()
            },
            AnalyticsConfig {
                clickhouse_max_execution_time_secs: 0,
                ..AnalyticsConfig::default()
            },
            AnalyticsConfig {
                clickhouse_max_rows_to_read: 0,
                ..AnalyticsConfig::default()
            },
            AnalyticsConfig {
                clickhouse_max_bytes_to_read: 0,
                ..AnalyticsConfig::default()
            },
        ] {
            config
                .validate()
                .expect_err("zero budget should be rejected");
        }
    }
}
