//! Durable lineage capture configuration.

use std::path::Path;

use moa_core::error::{MoaError, Result};
use serde::{Deserialize, Serialize};

const LOCAL_DEVELOPMENT_JOURNAL_PATH: &str = "~/.moa/lineage-journal";

/// Engineering-tier lineage capture configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LineageConfig {
    /// Whether durable lineage capture is enabled.
    pub enabled: bool,
    /// Bounded hot-path channel capacity.
    pub channel_capacity: usize,
    /// Maximum rows written per worker flush.
    pub batch_size: usize,
    /// Maximum age for a partial worker batch.
    pub batch_max_age_secs: u64,
    /// Durable fjall journal path.
    pub journal_path: String,
    /// Fraction of pgvector queries that run full EXPLAIN ANALYZE.
    pub sample_pgvector_explain: f64,
}

impl Default for LineageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age_secs: 2,
            journal_path: LOCAL_DEVELOPMENT_JOURNAL_PATH.to_string(),
            sample_pgvector_explain: 0.01,
        }
    }
}

impl LineageConfig {
    /// Validates whether the configured fjall journal path is durable enough.
    pub fn validate_journal_path(&self) -> Result<()> {
        let path = self.journal_path.trim();
        let parsed = Path::new(path);
        let invalid = path.is_empty()
            || path.starts_with('~')
            || parsed.is_relative()
            || parsed.starts_with("/tmp")
            || parsed.starts_with("/var/tmp")
            || path == LOCAL_DEVELOPMENT_JOURNAL_PATH;

        if invalid {
            return Err(MoaError::ConfigError(format!(
                "observability.lineage.journal_path `{path}` is not durable; use an explicitly persistent mounted path such as /var/lib/moa/lineage-journal for the Postgres lineage sink"
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_default_lineage_journal_path() {
        // Pins: Kubernetes audit durability cannot rely on the local development journal.
        let error = LineageConfig::default()
            .validate_journal_path()
            .expect_err("startup should reject the default lineage journal path");

        assert_eq!(
            error.to_string(),
            "configuration error: observability.lineage.journal_path `~/.moa/lineage-journal` is not durable; use an explicitly persistent mounted path such as /var/lib/moa/lineage-journal for the Postgres lineage sink"
        );
    }

    #[test]
    fn allows_explicit_persistent_lineage_journal_path() {
        // Pins: mounted absolute paths remain available for cloud lineage journaling.
        let config = LineageConfig {
            journal_path: "/var/lib/moa/lineage-journal".to_string(),
            ..LineageConfig::default()
        };

        config
            .validate_journal_path()
            .expect("explicit persistent lineage journal path should be allowed");
    }

    #[test]
    fn rejects_tmp_lineage_journal_path() {
        // Pins: pod-local tmp storage is not accepted for audit-durable lineage journaling.
        let config = LineageConfig {
            journal_path: "/tmp/moa-lineage-journal".to_string(),
            ..LineageConfig::default()
        };

        let error = config
            .validate_journal_path()
            .expect_err("startup should reject tmp lineage journal paths");

        assert_eq!(
            error.to_string(),
            "configuration error: observability.lineage.journal_path `/tmp/moa-lineage-journal` is not durable; use an explicitly persistent mounted path such as /var/lib/moa/lineage-journal for the Postgres lineage sink"
        );
    }

    #[test]
    fn rejects_empty_and_relative_lineage_journal_paths() {
        // Pins: Kubernetes lineage journaling must name a mounted absolute path.
        for path in ["", "relative/lineage-journal", "../lineage-journal"] {
            let config = LineageConfig {
                journal_path: path.to_string(),
                ..LineageConfig::default()
            };

            config
                .validate_journal_path()
                .expect_err("startup should reject empty and relative lineage journal paths");
        }
    }
}
