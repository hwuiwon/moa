//! In-memory runtime cache store.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::{
    BoundedLeaseDecision, RateTokenDecision, RetryBudgetDecision, RuntimeCacheStore,
};
use tokio::sync::RwLock;
use tokio::time::Instant;

/// Minimum interval between opportunistic sweeps of expired entries.
///
/// Keeping this coarse means a sweep costs at most one `HashMap::retain` per interval on the
/// write path, so steady-state writes do not pay an O(n) scan on every call.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Seconds a per-minute rate bucket refills over.
const SECONDS_PER_MINUTE: f64 = 60.0;

/// Process-local runtime cache backed by a Tokio `RwLock`.
#[derive(Debug, Default)]
pub struct MemoryRuntimeCacheStore {
    state: RwLock<State>,
}

#[derive(Debug)]
struct State {
    entries: HashMap<String, Entry>,
    lease_sets: HashMap<String, HashMap<String, Instant>>,
    buckets: HashMap<String, TokenBucket>,
    cooldowns: HashMap<String, Instant>,
    retry_windows: HashMap<String, RetryWindow>,
    last_sweep: Instant,
}

impl Default for State {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            lease_sets: HashMap::new(),
            buckets: HashMap::new(),
            cooldowns: HashMap::new(),
            retry_windows: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    expires_at: Instant,
}

/// One shared per-minute token bucket.
#[derive(Debug, Clone)]
struct TokenBucket {
    /// Bucket size, equal to the configured per-minute limit.
    capacity: f64,
    /// Tokens available at [`last_refill`](Self::last_refill).
    tokens: f64,
    /// When the bucket was last refilled.
    last_refill: Instant,
    /// When an idle bucket may be reclaimed.
    expires_at: Instant,
}

/// One shared sliding retry-budget window.
#[derive(Debug, Clone)]
struct RetryWindow {
    /// When the current window opened.
    started_at: Instant,
    /// Requests counted in the current window.
    requests: u64,
    /// Retries consumed in the current window.
    retries: u64,
    /// When an idle window may be reclaimed.
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
            state.lease_sets.retain(|_, leases| {
                leases.retain(|_, expires_at| *expires_at > now);
                !leases.is_empty()
            });
            state.buckets.retain(|_, bucket| bucket.expires_at > now);
            state.cooldowns.retain(|_, deadline| *deadline > now);
            state
                .retry_windows
                .retain(|_, window| window.expires_at > now);
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

    async fn try_acquire_bounded_lease(
        &self,
        key: &str,
        lease_id: &str,
        limit: usize,
        ttl: Duration,
    ) -> Result<BoundedLeaseDecision> {
        if limit == 0 {
            return Err(MoaError::ValidationError(
                "bounded lease limit must be greater than zero".to_string(),
            ));
        }
        let expires_at = Self::expires_at(ttl)?;
        let now = Instant::now();
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        let leases = state.lease_sets.entry(key.to_string()).or_default();
        leases.retain(|_, lease_expires_at| *lease_expires_at > now);
        if leases.contains_key(lease_id) {
            leases.insert(lease_id.to_string(), expires_at);
            return Ok(BoundedLeaseDecision {
                acquired: true,
                live: leases.len(),
            });
        }
        if leases.len() >= limit {
            return Ok(BoundedLeaseDecision {
                acquired: false,
                live: leases.len(),
            });
        }
        leases.insert(lease_id.to_string(), expires_at);
        Ok(BoundedLeaseDecision {
            acquired: true,
            live: leases.len(),
        })
    }

    async fn release_bounded_lease(&self, key: &str, lease_id: &str) -> Result<usize> {
        let now = Instant::now();
        let mut state = self.state.write().await;
        let Some(leases) = state.lease_sets.get_mut(key) else {
            return Ok(0);
        };
        leases.retain(|id, expires_at| id != lease_id && *expires_at > now);
        let live = leases.len();
        if leases.is_empty() {
            state.lease_sets.remove(key);
        }
        Ok(live)
    }

    async fn try_consume_rate_tokens(
        &self,
        key: &str,
        limit_per_min: u32,
        permits: u32,
        ttl: Duration,
    ) -> Result<RateTokenDecision> {
        if limit_per_min == 0 {
            return Err(MoaError::ValidationError(
                "rate token limit must be greater than zero".to_string(),
            ));
        }
        let capacity = f64::from(limit_per_min);
        let refill_per_sec = capacity / SECONDS_PER_MINUTE;
        let now = Instant::now();
        let expires_at = Self::expires_at(ttl)?;
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        let bucket = state
            .buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket {
                capacity,
                tokens: capacity,
                last_refill: now,
                expires_at,
            });
        // A reconfigured limit rebuilds the bucket rather than refilling an old
        // capacity, so a lowered ceiling takes effect on the next call.
        if bucket.capacity != capacity {
            *bucket = TokenBucket {
                capacity,
                tokens: capacity,
                last_refill: now,
                expires_at,
            };
        }
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(capacity);
            bucket.last_refill = now;
        }
        bucket.expires_at = expires_at;
        // A single demand larger than the whole bucket drains it instead of
        // waiting for a refill that can never arrive.
        let needed = f64::from(permits).min(capacity);
        if bucket.tokens >= needed {
            bucket.tokens -= needed;
            return Ok(RateTokenDecision {
                admitted: true,
                retry_after: Duration::ZERO,
            });
        }
        Ok(RateTokenDecision {
            admitted: false,
            retry_after: Duration::from_secs_f64((needed - bucket.tokens) / refill_per_sec),
        })
    }

    async fn extend_cooldown(&self, key: &str, cooldown: Duration) -> Result<Duration> {
        let now = Instant::now();
        let deadline = now.checked_add(cooldown).ok_or_else(|| {
            MoaError::ValidationError("runtime cache cooldown is too large".to_string())
        })?;
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        let current = state.cooldowns.entry(key.to_string()).or_insert(deadline);
        if *current < deadline {
            *current = deadline;
        }
        Ok(current.saturating_duration_since(now))
    }

    async fn cooldown_remaining(&self, key: &str) -> Result<Duration> {
        let now = Instant::now();
        let state = self.state.read().await;
        Ok(state
            .cooldowns
            .get(key)
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(Duration::ZERO))
    }

    async fn note_windowed_request(&self, key: &str, window: Duration) -> Result<u64> {
        let now = Instant::now();
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        let entry = rotated_window(&mut state.retry_windows, key, now, window)?;
        entry.requests = entry.requests.saturating_add(1);
        Ok(entry.requests)
    }

    async fn try_consume_retry_budget(
        &self,
        key: &str,
        window: Duration,
        budget_percent: u32,
        budget_floor: u64,
    ) -> Result<RetryBudgetDecision> {
        let now = Instant::now();
        let mut state = self.state.write().await;
        Self::sweep_expired(&mut state);
        let entry = rotated_window(&mut state.retry_windows, key, now, window)?;
        let budget = retry_budget(entry.requests, budget_percent, budget_floor);
        let allowed = entry.retries < budget;
        if allowed {
            entry.retries = entry.retries.saturating_add(1);
        }
        Ok(RetryBudgetDecision {
            allowed,
            requests: entry.requests,
            retries: entry.retries,
        })
    }
}

/// Returns the retry-budget window for `key`, opening a fresh one when the
/// current window has elapsed.
fn rotated_window<'a>(
    windows: &'a mut HashMap<String, RetryWindow>,
    key: &str,
    now: Instant,
    window: Duration,
) -> Result<&'a mut RetryWindow> {
    // Twice the window keeps a rotated-but-unread entry reclaimable without
    // discarding the window that is still being counted.
    let expires_at = now
        .checked_add(window.saturating_mul(2))
        .ok_or_else(|| MoaError::ValidationError("retry budget window is too large".to_string()))?;
    let fresh = RetryWindow {
        started_at: now,
        requests: 0,
        retries: 0,
        expires_at,
    };
    let entry = windows
        .entry(key.to_string())
        .or_insert_with(|| fresh.clone());
    if now.saturating_duration_since(entry.started_at) >= window {
        *entry = fresh;
    } else {
        entry.expires_at = expires_at;
    }
    Ok(entry)
}

/// Returns the retry allowance for one window: a percentage of observed request
/// volume, never below the floor that keeps low-volume callers retrying normally.
fn retry_budget(requests: u64, budget_percent: u32, budget_floor: u64) -> u64 {
    requests
        .saturating_mul(u64::from(budget_percent))
        .saturating_div(100)
        .max(budget_floor)
}
