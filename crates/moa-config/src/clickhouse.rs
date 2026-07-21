//! Optional ClickHouse analytics-store configuration.
//!
//! When this section is present, high-volume append-only analytics rows
//! (currently `turn_lineage`) are written to ClickHouse instead of Postgres.
//! When absent, everything stays in Postgres — presence of the section is the
//! only switch.

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};

/// Connection and retention settings for the optional ClickHouse store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClickHouseConfig {
    /// HTTP interface endpoint, for example `http://localhost:8123`.
    pub url: String,
    /// Target database; created at startup when missing.
    pub database: String,
    /// Optional user for HTTP basic auth.
    pub user: Option<String>,
    /// Optional password for HTTP basic auth.
    pub password: Option<String>,
    /// Row TTL in days for `turn_lineage`, mirroring the Postgres/Timescale
    /// 30-day retention drop.
    pub lineage_ttl_days: u32,
    /// Poll interval in seconds for the analytics exporter loop; also sets the
    /// cursor rewind overlap (`2 × export_poll_secs`).
    pub export_poll_secs: u64,
    /// Maximum rows pulled from Postgres and inserted into ClickHouse per
    /// analytics-export batch.
    pub export_batch_rows: usize,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            database: "moa".to_string(),
            user: None,
            password: None,
            lineage_ttl_days: 30,
            export_poll_secs: 15,
            export_batch_rows: 5000,
        }
    }
}

impl ClickHouseConfig {
    /// Validates that the section names a usable HTTP endpoint and database.
    pub fn validate(&self) -> Result<()> {
        let url = self.url.trim();
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(MoaError::ConfigError(format!(
                "clickhouse.url `{url}` must be an http(s) endpoint such as http://localhost:8123"
            )));
        }
        if self.database.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "clickhouse.database must be a non-empty database name".to_string(),
            ));
        }
        if self.lineage_ttl_days == 0 {
            return Err(MoaError::ConfigError(
                "clickhouse.lineage_ttl_days must be greater than zero".to_string(),
            ));
        }
        if self.export_poll_secs == 0 {
            return Err(MoaError::ConfigError(
                "clickhouse.export_poll_secs must be greater than zero".to_string(),
            ));
        }
        if self.export_batch_rows == 0 {
            return Err(MoaError::ConfigError(
                "clickhouse.export_batch_rows must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_http_endpoint_with_defaults() {
        // Pins: a bare `[clickhouse] url = ...` section is a complete, valid config.
        let config = ClickHouseConfig {
            url: "http://localhost:8123".to_string(),
            ..ClickHouseConfig::default()
        };

        config.validate().expect("http endpoint should validate");
        assert_eq!(config.database, "moa");
        assert_eq!(config.lineage_ttl_days, 30);
        assert_eq!(config.export_poll_secs, 15);
        assert_eq!(config.export_batch_rows, 5000);
    }

    #[test]
    fn rejects_missing_or_non_http_url() {
        // Pins: presence of the section without a usable endpoint fails startup
        // instead of silently falling back to Postgres.
        for url in ["", "localhost:8123", "tcp://localhost:9000"] {
            let config = ClickHouseConfig {
                url: url.to_string(),
                ..ClickHouseConfig::default()
            };

            let error = config
                .validate()
                .expect_err("non-http endpoints should be rejected");
            assert!(error.to_string().contains("clickhouse.url"));
        }
    }

    #[test]
    fn rejects_empty_database_and_zero_ttl() {
        let no_database = ClickHouseConfig {
            url: "http://localhost:8123".to_string(),
            database: " ".to_string(),
            ..ClickHouseConfig::default()
        };
        no_database
            .validate()
            .expect_err("blank database should be rejected");

        let zero_ttl = ClickHouseConfig {
            url: "http://localhost:8123".to_string(),
            lineage_ttl_days: 0,
            ..ClickHouseConfig::default()
        };
        zero_ttl
            .validate()
            .expect_err("zero retention should be rejected");
    }

    #[test]
    fn rejects_zero_export_knobs() {
        // Pins: the exporter loop cannot poll on a zero interval or insert
        // zero-row batches, so both knobs must be positive.
        let zero_poll = ClickHouseConfig {
            url: "http://localhost:8123".to_string(),
            export_poll_secs: 0,
            ..ClickHouseConfig::default()
        };
        let error = zero_poll
            .validate()
            .expect_err("zero poll interval should be rejected");
        assert!(error.to_string().contains("export_poll_secs"));

        let zero_batch = ClickHouseConfig {
            url: "http://localhost:8123".to_string(),
            export_batch_rows: 0,
            ..ClickHouseConfig::default()
        };
        let error = zero_batch
            .validate()
            .expect_err("zero batch size should be rejected");
        assert!(error.to_string().contains("export_batch_rows"));
    }
}
