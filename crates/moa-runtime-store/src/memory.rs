//! In-memory runtime cache store.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
use tokio::sync::RwLock;
use tokio::time::Instant;

/// Process-local runtime cache backed by a Tokio `RwLock`.
#[derive(Debug, Default)]
pub struct MemoryRuntimeCacheStore {
    entries: RwLock<HashMap<String, Entry>>,
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
}

#[async_trait]
impl RuntimeCacheStore for MemoryRuntimeCacheStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let now = Instant::now();
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(key) {
                if entry.expires_at > now {
                    return Ok(Some(entry.value.clone()));
                }
            } else {
                return Ok(None);
            }
        }

        let mut entries = self.entries.write().await;
        if entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= Instant::now())
        {
            entries.remove(key);
        }
        Ok(None)
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        let entry = Entry {
            value,
            expires_at: Self::expires_at(ttl)?,
        };
        self.entries.write().await.insert(key.to_string(), entry);
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.entries.write().await.remove(key);
        Ok(())
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool> {
        let mut entries = self.entries.write().await;
        if entries
            .get(key)
            .is_some_and(|entry| entry.expires_at <= Instant::now())
        {
            entries.remove(key);
        }

        let current = entries.get(key).map(|entry| entry.value.as_slice());
        if current != expected {
            return Ok(false);
        }

        entries.insert(
            key.to_string(),
            Entry {
                value,
                expires_at: Self::expires_at(ttl)?,
            },
        );
        Ok(true)
    }

    async fn expire(&self, key: &str, ttl: Duration) -> Result<()> {
        let mut entries = self.entries.write().await;
        match entries.get_mut(key) {
            Some(entry) if entry.expires_at > Instant::now() => {
                entry.expires_at = Self::expires_at(ttl)?;
            }
            Some(_) => {
                entries.remove(key);
            }
            None => {}
        }
        Ok(())
    }
}
