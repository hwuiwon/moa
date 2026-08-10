//! Portable sandbox-checkpoint storage, retention, and deletion configuration.

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};

use crate::ObjectStoreLocationConfig;

/// Required checkpoint-bucket versioning posture.
///
/// Portable checkpoint cleanup currently supports only buckets whose provider
/// reports versioning disabled. Versioned buckets fail closed because deleting
/// the current object would leave recoverable historical bytes behind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointBucketVersioningPolicy {
    /// Require a provider-verified unversioned bucket.
    #[default]
    UnversionedRequired,
}

/// Runtime observation policy for the checkpoint bucket's versioning state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointVersioningObservationConfig {
    /// Maximum age of the authenticated provider observation used for readiness.
    pub maximum_age_seconds: u64,
    /// Deadline for the authenticated versioning preflight request.
    pub timeout_seconds: u64,
}

impl Default for CheckpointVersioningObservationConfig {
    fn default() -> Self {
        Self {
            maximum_age_seconds: 60,
            timeout_seconds: 10,
        }
    }
}

/// Replica-consistent checkpoint retention and garbage-collection policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointRetentionConfig {
    /// Number of ancestors retained behind the committed head.
    pub retained_ancestor_count: u32,
    /// Minimum checkpoint age before it may be claimed for deletion.
    pub minimum_age_seconds: u64,
    /// Maximum checkpoints claimed by one GC transaction.
    pub gc_batch_size: u32,
    /// Lease duration for one durable GC claim.
    pub claim_ttl_seconds: u64,
    /// Delay before a failed deletion may be reclaimed.
    pub retry_backoff_seconds: u64,
}

impl Default for CheckpointRetentionConfig {
    fn default() -> Self {
        Self {
            retained_ancestor_count: 3,
            minimum_age_seconds: 86_400,
            gc_batch_size: 100,
            claim_ttl_seconds: 300,
            retry_backoff_seconds: 60,
        }
    }
}

/// Bounds and consistency window for exact-prefix checkpoint deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointDeletionConfig {
    /// Maximum objects accepted beneath one checkpoint prefix.
    pub max_objects: usize,
    /// Maximum aggregate stored bytes accepted beneath one checkpoint prefix.
    pub max_bytes: u64,
    /// Minimum delay between the two empty observations proving absence.
    pub consistency_window_seconds: u64,
}

impl Default for CheckpointDeletionConfig {
    fn default() -> Self {
        Self {
            max_objects: 200_000,
            max_bytes: 16 * 1024 * 1024 * 1024,
            consistency_window_seconds: 1,
        }
    }
}

/// Restate inactivity budget for the current bounded tenant-purge workflow.
pub const SANDBOX_TENANT_PURGE_INACTIVITY_TIMEOUT_SECONDS: u64 = 6 * 60;

/// Maximum separated absence windows in one current tenant-purge external phase.
pub const SANDBOX_TENANT_PURGE_SEPARATED_ABSENCE_WINDOWS: u64 = 3;

/// Durable portable-checkpoint storage and archive safety limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxCheckpointConfig {
    /// Whether persistent workspace checkpoint publication is enabled.
    pub enabled: bool,
    /// Owner-specific bucket and namespace within the shared object store.
    pub storage: ObjectStoreLocationConfig,
    /// Required bucket versioning posture.
    pub bucket_versioning: CheckpointBucketVersioningPolicy,
    /// Authenticated versioning observation required before readiness and purge.
    pub versioning_observation: CheckpointVersioningObservationConfig,
    /// Retention and durable GC policy shared by every replica.
    pub retention: CheckpointRetentionConfig,
    /// Exact-prefix deletion bounds and absence-proof window.
    pub deletion: CheckpointDeletionConfig,
    /// Maximum filesystem entry count per checkpoint.
    pub max_entries: usize,
    /// Maximum normalized filesystem path depth.
    pub max_path_depth: usize,
    /// Maximum bytes in one regular file.
    pub max_file_bytes: u64,
    /// Maximum logical bytes across one checkpoint.
    pub max_total_bytes: u64,
    /// Maximum decompressed bytes in one independently encrypted chunk.
    pub max_chunk_bytes: usize,
    /// Maximum compressed bytes accepted for one chunk.
    pub max_compressed_chunk_bytes: usize,
}

impl Default for SandboxCheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            storage: ObjectStoreLocationConfig {
                bucket: "moa-workspace-checkpoints".to_string(),
                prefix: "workspace-checkpoints".to_string(),
            },
            bucket_versioning: CheckpointBucketVersioningPolicy::default(),
            versioning_observation: CheckpointVersioningObservationConfig::default(),
            retention: CheckpointRetentionConfig::default(),
            deletion: CheckpointDeletionConfig::default(),
            max_entries: 100_000,
            max_path_depth: 64,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 4 * 1024 * 1024 * 1024,
            max_chunk_bytes: 4 * 1024 * 1024,
            max_compressed_chunk_bytes: 8 * 1024 * 1024,
        }
    }
}

impl SandboxCheckpointConfig {
    /// Validates the durable namespace and all fail-closed archive limits.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.storage.validate("sandbox_checkpoints.storage")?;
        if self.versioning_observation.maximum_age_seconds == 0
            || self.versioning_observation.timeout_seconds == 0
            || self.versioning_observation.timeout_seconds
                > self.versioning_observation.maximum_age_seconds
        {
            return Err(MoaError::ConfigError(
                "sandbox checkpoint versioning observation requires a positive timeout no greater than its maximum age"
                    .to_string(),
            ));
        }
        if self.max_entries == 0
            || self.max_path_depth == 0
            || self.max_file_bytes == 0
            || self.max_total_bytes == 0
            || self.max_chunk_bytes == 0
            || self.max_compressed_chunk_bytes == 0
        {
            return Err(MoaError::ConfigError(
                "sandbox checkpoint limits must all be greater than zero".to_string(),
            ));
        }
        let retention = &self.retention;
        if retention.retained_ancestor_count == 0
            || retention.minimum_age_seconds == 0
            || retention.gc_batch_size == 0
            || retention.claim_ttl_seconds == 0
            || retention.retry_backoff_seconds == 0
            || retention.minimum_age_seconds > i64::MAX as u64
            || retention.claim_ttl_seconds > i64::MAX as u64
            || retention.retry_backoff_seconds > i64::MAX as u64
        {
            return Err(MoaError::ConfigError(
                "sandbox checkpoint retention values must be nonzero and fit signed duration arithmetic"
                    .to_string(),
            ));
        }
        let deletion = &self.deletion;
        let tenant_purge_minimum_sleep = deletion
            .consistency_window_seconds
            .checked_mul(SANDBOX_TENANT_PURGE_SEPARATED_ABSENCE_WINDOWS);
        if deletion.max_objects == 0
            || deletion.max_bytes == 0
            || deletion.consistency_window_seconds == 0
            || deletion.max_bytes < self.max_total_bytes
            || deletion.consistency_window_seconds > retention.claim_ttl_seconds
            || tenant_purge_minimum_sleep
                .is_none_or(|seconds| seconds >= SANDBOX_TENANT_PURGE_INACTIVITY_TIMEOUT_SECONDS)
            || self.max_file_bytes > self.max_total_bytes
            || self.max_chunk_bytes as u64 > self.max_file_bytes
            || self.max_compressed_chunk_bytes < self.max_chunk_bytes
        {
            return Err(MoaError::ConfigError(
                "sandbox checkpoint retention, archive, and deletion bounds are inconsistent"
                    .to_string(),
            ));
        }
        Ok(())
    }
}
