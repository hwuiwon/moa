//! Runtime cache configuration.

use serde::{Deserialize, Serialize};

/// Runtime cache backend selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCacheBackend {
    /// Select Redis when a Redis URL is configured, otherwise use memory.
    #[default]
    Auto,
    /// Use a process-local in-memory cache.
    Memory,
    /// Use Redis for shared runtime coordination.
    Redis,
}

/// Configuration for ephemeral runtime cache state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeCacheConfig {
    /// Backend used for runtime cache operations.
    pub backend: RuntimeCacheBackend,
    /// Redis URL used when the Redis backend is selected.
    pub redis_url: Option<String>,
}

impl Default for RuntimeCacheConfig {
    fn default() -> Self {
        Self {
            backend: RuntimeCacheBackend::Auto,
            redis_url: None,
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies runtime cache environment overrides.
    pub(in crate::config) fn apply_runtime_cache_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.runtime_cache.backend,
            self.runtime_cache_backend,
        );
        set_option_if_some(
            &mut config.runtime_cache.redis_url,
            &self.runtime_cache_redis_url,
        );
    }
}
