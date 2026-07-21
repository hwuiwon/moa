//! Runtime cache store implementations.

mod memory;

#[cfg(feature = "redis")]
mod redis;

pub use memory::MemoryRuntimeCacheStore;

#[cfg(feature = "redis")]
pub use redis::RedisRuntimeCacheStore;

use moa_config::{RuntimeCacheBackend, RuntimeCacheConfig};
use moa_core::error::{MoaError, Result};

/// Environment flag that explicitly allows the process-local memory backend for `auto`.
pub(crate) const MEMORY_BACKEND_OPT_IN_ENV: &str = "MOA_RUNTIME_CACHE_ALLOW_MEMORY";

/// Runtime cache backend after `auto` selection has been resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedRuntimeCacheBackend {
    /// Use a process-local in-memory cache.
    Memory,
    /// Use Redis for shared runtime coordination.
    Redis,
}

/// Resolves the configured runtime cache backend, failing closed for distributed safety.
///
/// The process-local [`ResolvedRuntimeCacheBackend::Memory`] backend is only selected when the
/// operator explicitly opts in — either `runtime_cache.backend = "memory"` or the
/// [`MEMORY_BACKEND_OPT_IN_ENV`] flag. An `auto` backend with no Redis URL and no opt-in returns a
/// clear error instead of silently falling back to a process-local cache, which would make
/// cross-instance coordination wrong in a fleet.
pub fn select_runtime_cache_backend(
    config: &RuntimeCacheConfig,
) -> Result<ResolvedRuntimeCacheBackend> {
    resolve_runtime_cache_backend(
        config.backend,
        has_redis_url(config),
        memory_backend_opt_in(),
    )
}

/// Pure backend resolution, split out so the fail-closed matrix is testable without env or config.
fn resolve_runtime_cache_backend(
    backend: RuntimeCacheBackend,
    has_redis_url: bool,
    memory_opt_in: bool,
) -> Result<ResolvedRuntimeCacheBackend> {
    match backend {
        RuntimeCacheBackend::Redis => Ok(ResolvedRuntimeCacheBackend::Redis),
        // Explicit operator opt-in to the process-local backend.
        RuntimeCacheBackend::Memory => Ok(ResolvedRuntimeCacheBackend::Memory),
        RuntimeCacheBackend::Auto if has_redis_url => Ok(ResolvedRuntimeCacheBackend::Redis),
        RuntimeCacheBackend::Auto if memory_opt_in => Ok(ResolvedRuntimeCacheBackend::Memory),
        RuntimeCacheBackend::Auto => Err(MoaError::ConfigError(format!(
            "runtime_cache.backend is 'auto' but no Redis URL is configured; set \
             runtime_cache.redis_url for distributed coordination, or explicitly opt into the \
             process-local cache with runtime_cache.backend = \"memory\" or {MEMORY_BACKEND_OPT_IN_ENV}=1"
        ))),
    }
}

fn memory_backend_opt_in() -> bool {
    std::env::var(MEMORY_BACKEND_OPT_IN_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE"))
        .unwrap_or(false)
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

    use moa_config::{RuntimeCacheBackend, RuntimeCacheConfig};
    use moa_core::error::Result;
    use moa_core::traits::RuntimeCacheStore;
    use tokio::time::advance;

    use super::{
        MemoryRuntimeCacheStore, ResolvedRuntimeCacheBackend, resolve_runtime_cache_backend,
        select_runtime_cache_backend,
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
    async fn memory_store_sweeps_expired_entries_never_read_again() -> Result<()> {
        // Pins: entries that expire and are never read again are reclaimed by the periodic sweep
        // on the write path, so the memory backend does not grow unboundedly.
        let store = MemoryRuntimeCacheStore::default();

        store
            .set("ghost", b"x".to_vec(), Duration::from_secs(1))
            .await?;
        assert_eq!(store.entry_count().await, 1);

        // Let the entry expire and cross the sweep interval without ever reading "ghost".
        advance(Duration::from_secs(60)).await;

        // A write on a different key triggers the sweep, which drops the stale entry.
        store
            .set("live", b"y".to_vec(), Duration::from_secs(300))
            .await?;
        assert_eq!(
            store.entry_count().await,
            1,
            "only the live key should remain after the sweep"
        );
        assert_eq!(store.get("live").await?, Some(b"y".to_vec()));
        assert_eq!(store.get("ghost").await?, None);

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
    fn backend_resolution_matrix_fails_closed_without_opt_in() {
        // Pins: `auto` only resolves to the process-local memory backend with an explicit opt-in;
        // otherwise it fails closed rather than silently degrading fleet coordination. Driven
        // through the pure resolver so the opt-in flag is deterministic (no env/config races).
        use ResolvedRuntimeCacheBackend::{Memory, Redis};

        // Explicit backends are honored regardless of URL/opt-in.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Redis, false, false).unwrap(),
            Redis
        );
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Memory, false, false).unwrap(),
            Memory
        );
        // `auto` prefers Redis when a URL is present.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, true, false).unwrap(),
            Redis
        );
        // `auto` without a URL yields memory only with an explicit opt-in.
        assert_eq!(
            resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, false, true).unwrap(),
            Memory
        );
        // `auto` without a URL and without opt-in fails closed with an actionable error.
        let error = resolve_runtime_cache_backend(RuntimeCacheBackend::Auto, false, false)
            .expect_err("auto without redis url or opt-in must fail closed");
        let message = error.to_string();
        assert!(
            message.contains("redis_url") && message.contains(super::MEMORY_BACKEND_OPT_IN_ENV),
            "error should tell the operator how to fix it: {message}"
        );
    }

    #[test]
    fn select_backend_reads_config_and_prefers_redis() {
        // Pins: the config-driven selector honors an explicit URL and explicit backends without
        // depending on the opt-in environment flag.
        let auto_with_url = RuntimeCacheConfig {
            redis_url: Some("redis://cache.example:6379/0".to_string()),
            ..RuntimeCacheConfig::default()
        };
        assert_eq!(
            select_runtime_cache_backend(&auto_with_url).unwrap(),
            ResolvedRuntimeCacheBackend::Redis
        );

        let explicit_memory = RuntimeCacheConfig {
            backend: RuntimeCacheBackend::Memory,
            redis_url: None,
        };
        assert_eq!(
            select_runtime_cache_backend(&explicit_memory).unwrap(),
            ResolvedRuntimeCacheBackend::Memory
        );

        let explicit_redis = RuntimeCacheConfig {
            backend: RuntimeCacheBackend::Redis,
            redis_url: None,
        };
        assert_eq!(
            select_runtime_cache_backend(&explicit_redis).unwrap(),
            ResolvedRuntimeCacheBackend::Redis
        );
    }
}
