//! Durable lineage capture configuration.

use moa_core::error::{MoaError, Result};
use serde::{Deserialize, Serialize};

/// Engineering-tier lineage capture configuration.
///
/// Acceptance is owned by Postgres (`analytics.lineage_journal`), so there is no
/// local path to configure and nothing about durability depends on the
/// filesystem a replica happens to be scheduled onto. The previous local-path
/// key, and the startup validation that tried to tell a durable mount from a
/// pod-local one, are both gone: no local directory could have been durable
/// across a rollout, so no value for that key was ever correct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LineageConfig {
    /// Bounded best-effort ingress channel capacity.
    pub channel_capacity: usize,
    /// Maximum ingress events committed to the queue per batch.
    pub batch_size: usize,
    /// Maximum age for a partial ingress batch, and the drain poll cadence.
    pub batch_max_age_secs: u64,
    /// Maximum queue rows claimed by one drain.
    pub claim_batch_size: usize,
    /// Claim lease lifetime.
    ///
    /// A replica that dies mid-batch holds its rows for at most this long before
    /// they become claimable again, so this is the worst-case recovery delay for
    /// an ungraceful pod termination.
    pub lease_ttl_secs: u64,
    /// Oldest claimable backlog age tolerated before readiness fails.
    ///
    /// Readiness, not liveness: a replica whose queue is this far behind should
    /// stop taking traffic, but restarting it would drop its leases and make the
    /// backlog worse.
    pub max_pending_age_secs: u64,
    /// Upper bound on the shutdown drain.
    ///
    /// Exceeding it is not data loss. Rows are committed in Postgres; the
    /// terminating replica simply stops working on them.
    pub drain_timeout_secs: u64,
    /// Fraction of pgvector queries that run full EXPLAIN ANALYZE.
    pub sample_pgvector_explain: f64,
}

impl Default for LineageConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age_secs: 2,
            claim_batch_size: 512,
            lease_ttl_secs: 60,
            max_pending_age_secs: 300,
            drain_timeout_secs: 30,
            sample_pgvector_explain: 0.01,
        }
    }
}

impl LineageConfig {
    /// Maximum number of lineage rows accepted or claimed in one batch.
    ///
    /// This is eight times the production default: enough headroom for deliberate
    /// throughput tuning while bounding the writer's eager allocation, one
    /// acceptance statement, and one destruction-lock array.
    pub const MAX_BATCH_SIZE: usize = 4_096;

    /// Refuses unbounded batches and zero-sized runtime controls.
    pub fn validate(&self) -> Result<()> {
        let invalid = [
            (self.channel_capacity == 0, "channel_capacity"),
            (self.batch_size == 0, "batch_size"),
            (self.batch_max_age_secs == 0, "batch_max_age_secs"),
            (self.claim_batch_size == 0, "claim_batch_size"),
            (self.lease_ttl_secs == 0, "lease_ttl_secs"),
            (self.max_pending_age_secs == 0, "max_pending_age_secs"),
            (self.drain_timeout_secs == 0, "drain_timeout_secs"),
        ]
        .into_iter()
        .find_map(|(is_invalid, field)| is_invalid.then_some(field));

        match invalid {
            Some(field) => Err(MoaError::ConfigError(format!(
                "observability.lineage.{field} must be greater than zero"
            ))),
            None if self.batch_size > Self::MAX_BATCH_SIZE => Err(MoaError::ConfigError(format!(
                "observability.lineage.batch_size must be at most {}",
                Self::MAX_BATCH_SIZE
            ))),
            None if self.claim_batch_size > Self::MAX_BATCH_SIZE => {
                Err(MoaError::ConfigError(format!(
                    "observability.lineage.claim_batch_size must be at most {}",
                    Self::MAX_BATCH_SIZE
                )))
            }
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LineageConfig;

    #[test]
    fn zero_runtime_knobs_are_rejected() {
        // Pins: every value passed to `mpsc::channel`, `interval`, queue claims,
        // leases, readiness, or bounded shutdown is strictly positive.
        for field in [
            "channel_capacity",
            "batch_size",
            "batch_max_age_secs",
            "claim_batch_size",
            "lease_ttl_secs",
            "max_pending_age_secs",
            "drain_timeout_secs",
        ] {
            let mut config = LineageConfig::default();
            match field {
                "channel_capacity" => config.channel_capacity = 0,
                "batch_size" => config.batch_size = 0,
                "batch_max_age_secs" => config.batch_max_age_secs = 0,
                "claim_batch_size" => config.claim_batch_size = 0,
                "lease_ttl_secs" => config.lease_ttl_secs = 0,
                "max_pending_age_secs" => config.max_pending_age_secs = 0,
                "drain_timeout_secs" => config.drain_timeout_secs = 0,
                _ => unreachable!("the test table contains only known lineage fields"),
            }

            let error = config.validate().expect_err("zero must be rejected");
            assert_eq!(
                error.to_string(),
                format!(
                    "configuration error: observability.lineage.{field} must be greater than zero"
                )
            );
        }
    }

    #[test]
    fn lineage_batches_are_bounded_at_the_documented_maximum() {
        // Pins: configured ingress allocations and claim lock arrays cannot be
        // made arbitrarily large, while the documented maximum remains usable.
        let mut config = LineageConfig {
            batch_size: LineageConfig::MAX_BATCH_SIZE,
            claim_batch_size: LineageConfig::MAX_BATCH_SIZE,
            ..LineageConfig::default()
        };
        config
            .validate()
            .expect("the documented lineage batch maximum should be valid");

        for field in ["batch_size", "claim_batch_size"] {
            match field {
                "batch_size" => config.batch_size = LineageConfig::MAX_BATCH_SIZE + 1,
                "claim_batch_size" => {
                    config.batch_size = LineageConfig::MAX_BATCH_SIZE;
                    config.claim_batch_size = LineageConfig::MAX_BATCH_SIZE + 1;
                }
                _ => unreachable!("the test table contains only bounded lineage fields"),
            }

            let error = config
                .validate()
                .expect_err("a lineage batch above the maximum must be rejected");
            assert_eq!(
                error.to_string(),
                format!(
                    "configuration error: observability.lineage.{field} must be at most {}",
                    LineageConfig::MAX_BATCH_SIZE
                )
            );
        }
    }
}
