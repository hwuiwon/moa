//! In-memory runtime cache store.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
use tokio::sync::RwLock;
use tokio::time::Instant;

/// Minimum interval between opportunistic sweeps of expired entries.
///
/// Keeping this coarse means a sweep costs at most one `HashMap::retain` per interval on the
/// write path, so steady-state writes do not pay an O(n) scan on every call.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Process-local runtime cache backed by a Tokio `RwLock`.
#[derive(Debug, Default)]
pub struct MemoryRuntimeCacheStore {
    state: RwLock<State>,
}

#[derive(Debug)]
struct State {
    entries: HashMap<String, Entry>,
    last_sweep: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    expires_at: Instant,
}

impl MemoryRuntimeCacheStore {
    /// Creates an empty in-memory runtime cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn expires_at(ttl: Duration) -> Result<Instant> {
        Instant::now()
            .checked_add(ttl)
            .ok_or_else(|| MoaError::ValidationError("runtime cache TTL is too large".to_string()))
    }

    /// Drops expired entries when at least [`SWEEP_INTERVAL`] has elapsed since the last sweep.
    ///
    /// Lazy expiry on read only reclaims keys that are read again; this sweep bounds growth from
    /// keys that are written once and never touched again. It runs on the write path (which
    /// already holds the lock and is the only path that grows the map), so no background task is
    /// needed for a process-local dev cache.
    fn sweep_expired(state: &mut State) {
        let now = Instant::now();
        if now.duration_since(state.last_sweep) >= SWEEP_INTERVAL {
            state.entries.retain(|_, entry| entry.expires_at > now);
            state.last_sweep = now;
        }
    }

    #[cfg(test)]
    pub(crate) async fn entry_count(&self) -> usize {
        self.state.read().await.entries.len()
    }
}

#[async_trait]
impl RuntimeCacheStore for MemoryRuntimeCacheStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        {
            let state = self.state.read().await;
            if let Some(entry) = state.entries.get(key) {
                if entry.expires_at > now {
                    return Ok(Some(entry.value.clone()));
                }
            } else {
                return Ok(None);
            }
        }

        let mut state = self.state.write().await;
        if state
            .entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= Instant::now())
        {
            state.entries.remove(key);
        }
        Ok(None)
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        let entry = Entry {
            value,
            expires_at: Self::expires_at(ttl)?,
        };
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        state.entries.insert(key.to_string(), entry);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.state.write().await.entries.remove(key);
        Ok(())
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool> {
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        if state
            .entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= Instant::now())
        {
            state.entries.remove(key);
        }

        let current = state.entries.get(key).map(|entry| entry.value.as_slice());
        if current != expected {
            return Ok(false);
        }

        state.entries.insert(
            key.to_string(),
            Entry {
                value,
                expires_at: Self::expires_at(ttl)?,
            },
        );
        Ok(true)
    }

    async fn expire(&self, key: &str, ttl: Duration) -> Result<()> {
        let mut state = self.state.write().await;
        match state.entries.get_mut(key) {
            Some(entry) if entry.expires_at > Instant::now() => {
                entry.expires_at = Self::expires_at(ttl)?;
            }
            Some(_) => {
                state.entries.remove(key);
            }
            None => {}
        }
        Ok(())
    }
}
