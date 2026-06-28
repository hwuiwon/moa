//! Runtime cache trait for ephemeral coordination state.

use std::time::Duration;

use async_trait::async_trait;

use crate::Result;

/// Ephemeral byte-value cache used for runtime coordination.
#[async_trait]
pub trait RuntimeCacheStore: Send + Sync {
    /// Loads one cache value when the key exists and has not expired.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;

    /// Stores one cache value with a time-to-live.
    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()>;

    /// Removes one cache value when present.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Replaces a cache value when the current value matches the expected bytes.
    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool>;

    /// Updates the expiration for an existing unexpired cache value.
    async fn expire(&self, key: &str, ttl: Duration) -> Result<()>;
}
