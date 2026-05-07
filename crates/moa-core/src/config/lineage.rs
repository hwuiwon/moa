//! Durable lineage capture configuration.

use serde::{Deserialize, Serialize};

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
            journal_path: "~/.moa/lineage-journal".to_string(),
            sample_pgvector_explain: 0.01,
        }
    }
}
