//! Durable lineage capture configuration.

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
#[serde(default)]
pub struct LineageConfig {
    /// Whether durable lineage capture is enabled.
    pub enabled: bool,
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
            enabled: false,
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
