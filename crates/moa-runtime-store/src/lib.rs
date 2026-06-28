//! Runtime cache store implementations.

mod memory;

#[cfg(feature = "redis")]
mod redis;

pub use memory::MemoryRuntimeCacheStore;

#[cfg(feature = "redis")]
pub use redis::RedisRuntimeCacheStore;

use moa_core::config::{RuntimeCacheBackend, RuntimeCacheConfig};

/// Runtime cache backend after `auto` selection has been resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedRuntimeCacheBackend {
    /// Use a process-local in-memory cache.
    Memory,
    /// Use Redis for shared runtime coordination.
    Redis,
}

/// Resolves the configured runtime cache backend.
#[must_use]
pub fn select_runtime_cache_backend(config: &RuntimeCacheConfig) -> ResolvedRuntimeCacheBackend {
    match config.backend {
        RuntimeCacheBackend::Auto if has_redis_url(config) => ResolvedRuntimeCacheBackend::Redis,
        RuntimeCacheBackend::Auto | RuntimeCacheBackend::Memory => {
            ResolvedRuntimeCacheBackend::Memory
        }
        RuntimeCacheBackend::Redis => ResolvedRuntimeCacheBackend::Redis,
    }
}

fn has_redis_url(config: &RuntimeCacheConfig) -> bool {
    config
        .redis_url
        .as_deref()
        .is_some_and(|url| !url.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::Result;
    use moa_core::config::{RuntimeCacheBackend, RuntimeCacheConfig};
    use moa_core::traits::RuntimeCacheStore;
    use tokio::time::advance;

    use super::{
        MemoryRuntimeCacheStore, ResolvedRuntimeCacheBackend, select_runtime_cache_backend,
    };

    #[tokio::test(start_paused = true)]
    async fn memory_store_sets_gets_expires_and_deletes_values() -> Result<()> {
        // Pins: memory runtime cache values obey set, expire, TTL cleanup, and delete.
        let store = MemoryRuntimeCacheStore::default();

        store
            .set("session:one", b"first".to_vec(), Duration::from_secs(10))
            .await?;
        assert_eq!(store.get("session:one").await?, Some(b"first".to_vec()));

        store.expire("session:one", Duration::from_secs(2)).await?;
        advance(Duration::from_secs(3)).await;
        assert_eq!(store.get("session:one").await?, None);

        store
            .set("session:one", b"second".to_vec(), Duration::from_secs(10))
            .await?;
        store.delete("session:one").await?;
        assert_eq!(store.get("session:one").await?, None);

        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn memory_compare_and_set_matches_absent_and_exact_values() -> Result<()> {
        // Pins: compare-and-set only mutates when the expected bytes match current cache state.
        let store = MemoryRuntimeCacheStore::default();

        assert!(
            store
                .compare_and_set("slot", None, b"one".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert!(
            !store
                .compare_and_set("slot", None, b"two".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"one".to_vec()));

        assert!(
            !store
                .compare_and_set(
                    "slot",
                    Some(b"wrong"),
                    b"two".to_vec(),
                    Duration::from_secs(10),
                )
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"one".to_vec()));

        assert!(
            store
                .compare_and_set(
                    "slot",
                    Some(b"one"),
                    b"two".to_vec(),
                    Duration::from_secs(1),
                )
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"two".to_vec()));

        advance(Duration::from_secs(2)).await;
        assert!(
            store
                .compare_and_set("slot", None, b"three".to_vec(), Duration::from_secs(10))
                .await?
        );
        assert_eq!(store.get("slot").await?, Some(b"three".to_vec()));

        Ok(())
    }

    #[test]
    fn backend_selection_uses_redis_only_when_auto_has_url() {
        // Pins: auto uses Redis only when a non-empty Redis URL is configured.
        let mut config = RuntimeCacheConfig::default();
        assert_eq!(
            select_runtime_cache_backend(&config),
            ResolvedRuntimeCacheBackend::Memory
        );

        config.redis_url = Some("redis://cache.example:6379/0".to_string());
        assert_eq!(
            select_runtime_cache_backend(&config),
            ResolvedRuntimeCacheBackend::Redis
        );

        config.redis_url = Some("   ".to_string());
        assert_eq!(
            select_runtime_cache_backend(&config),
            ResolvedRuntimeCacheBackend::Memory
        );

        config.backend = RuntimeCacheBackend::Memory;
        config.redis_url = Some("redis://cache.example:6379/0".to_string());
        assert_eq!(
            select_runtime_cache_backend(&config),
            ResolvedRuntimeCacheBackend::Memory
        );

        config.backend = RuntimeCacheBackend::Redis;
        config.redis_url = None;
        assert_eq!(
            select_runtime_cache_backend(&config),
            ResolvedRuntimeCacheBackend::Redis
        );
    }
}
