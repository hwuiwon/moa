//! Redis runtime cache store.

use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::{
    BoundedLeaseDecision, RateTokenDecision, RetryBudgetDecision, RuntimeCacheStore,
};
use redis::AsyncCommands;

/// Redis-backed runtime cache used for shared coordination state.
#[derive(Clone)]
pub struct RedisRuntimeCacheStore {
    connection: redis::aio::MultiplexedConnection,
}

impl RedisRuntimeCacheStore {
    /// Creates a Redis runtime cache from a Redis URL and verifies connectivity.
    pub async fn new(redis_url: &str) -> Result<Self> {
        let client = redis::Client::open(redis_url)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        let mut connection = client
            .get_multiplexed_async_connection()
            .await
            .map_err(|error| {
                MoaError::ConfigError(format!("connect to Redis runtime cache: {error}"))
            })?;
        let pong: String = redis::cmd("PING")
            .query_async(&mut connection)
            .await
            .map_err(|error| MoaError::ConfigError(format!("ping Redis runtime cache: {error}")))?;
        if pong != "PONG" {
            return Err(MoaError::ConfigError(format!(
                "Redis runtime cache PING returned {pong:?}"
            )));
        }
        Ok(Self { connection })
    }
}

#[async_trait]
impl RuntimeCacheStore for RedisRuntimeCacheStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut connection = self.connection.clone();
        connection.get(key).await.map_err(map_redis_error)
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        let mut connection = self.connection.clone();
        redis::cmd("SET")
            .arg(key)
            .arg(value)
            .arg("PX")
            .arg(ttl_millis(ttl)?)
            .query_async(&mut connection)
            .await
            .map_err(map_redis_error)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let mut connection = self.connection.clone();
        let _: usize = connection.del(key).await.map_err(map_redis_error)?;
        Ok(())
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: Vec<u8>,
        ttl: Duration,
    ) -> Result<bool> {
        let mut connection = self.connection.clone();
        let expect_absent = expected.is_none();
        let expected = expected.unwrap_or_default();
        let script = redis::Script::new(
            r#"
            local current = redis.call("GET", KEYS[1])
            if ARGV[1] == "1" then
                if current ~= false then
                    return 0
                end
            else
                if current ~= ARGV[2] then
                    return 0
                end
            end
            redis.call("SET", KEYS[1], ARGV[3], "PX", ARGV[4])
            return 1
            "#,
        );
        let changed: i32 = script
            .key(key)
            .arg(if expect_absent { "1" } else { "0" })
            .arg(expected)
            .arg(value)
            .arg(ttl_millis(ttl)?)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(changed == 1)
    }

    async fn expire(&self, key: &str, ttl: Duration) -> Result<()> {
        let mut connection = self.connection.clone();
        let _: bool = connection
            .pexpire(key, ttl_millis(ttl)?)
            .await
            .map_err(map_redis_error)?;
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
        let ttl_ms = ttl_millis(ttl)?;
        let limit: i64 = limit.try_into().map_err(|_| {
            MoaError::ValidationError("bounded lease limit is too large".to_string())
        })?;
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            redis.call("ZREMRANGEBYSCORE", KEYS[1], "-inf", now_ms)
            local existing = redis.call("ZSCORE", KEYS[1], ARGV[1])
            local live = redis.call("ZCARD", KEYS[1])
            if existing == false and live >= tonumber(ARGV[2]) then
                return {0, live}
            end
            redis.call("ZADD", KEYS[1], now_ms + tonumber(ARGV[3]), ARGV[1])
            redis.call("PEXPIRE", KEYS[1], ARGV[3])
            return {1, redis.call("ZCARD", KEYS[1])}
            "#,
        );
        let (acquired, live): (i64, i64) = script
            .key(key)
            .arg(lease_id)
            .arg(limit)
            .arg(ttl_ms)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(BoundedLeaseDecision {
            acquired: acquired == 1,
            live: live.try_into().map_err(|_| {
                MoaError::StorageError("Redis returned a negative bounded lease count".to_string())
            })?,
        })
    }

    async fn release_bounded_lease(&self, key: &str, lease_id: &str) -> Result<usize> {
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            redis.call("ZREMRANGEBYSCORE", KEYS[1], "-inf", now_ms)
            redis.call("ZREM", KEYS[1], ARGV[1])
            local live = redis.call("ZCARD", KEYS[1])
            if live == 0 then redis.call("DEL", KEYS[1]) end
            return live
            "#,
        );
        let live: i64 = script
            .key(key)
            .arg(lease_id)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        live.try_into().map_err(|_| {
            MoaError::StorageError("Redis returned a negative bounded lease count".to_string())
        })
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
        let ttl_ms = ttl_millis(ttl)?;
        let mut connection = self.connection.clone();
        // Refill is derived from the server clock inside the script, so replicas
        // with skewed clocks cannot each refill the shared bucket on their own
        // reading of "now".
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            local capacity = tonumber(ARGV[1])
            local permits = math.min(tonumber(ARGV[2]), capacity)
            local ttl_ms = tonumber(ARGV[3])
            local refill_per_ms = capacity / 60000.0
            local stored = redis.call("HMGET", KEYS[1], "tokens", "at", "cap")
            local tokens = tonumber(stored[1])
            local at = tonumber(stored[2])
            local cap = tonumber(stored[3])
            if tokens == nil or at == nil or cap ~= capacity then
                tokens = capacity
                at = now_ms
            end
            local elapsed = now_ms - at
            if elapsed > 0 then
                tokens = math.min(capacity, tokens + (elapsed * refill_per_ms))
            end
            local admitted = 0
            local retry_after = 0
            if tokens >= permits then
                tokens = tokens - permits
                admitted = 1
            else
                retry_after = math.ceil((permits - tokens) / refill_per_ms)
            end
            redis.call("HSET", KEYS[1], "tokens", tostring(tokens), "at", now_ms, "cap", capacity)
            redis.call("PEXPIRE", KEYS[1], ttl_ms)
            return {admitted, retry_after}
            "#,
        );
        let (admitted, retry_after_ms): (i64, i64) = script
            .key(key)
            .arg(f64::from(limit_per_min))
            .arg(f64::from(permits))
            .arg(ttl_ms)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(RateTokenDecision {
            admitted: admitted == 1,
            retry_after: Duration::from_millis(retry_after_ms.max(0) as u64),
        })
    }

    async fn extend_cooldown(&self, key: &str, cooldown: Duration) -> Result<Duration> {
        let cooldown_ms = ttl_millis(cooldown)?;
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            local deadline = now_ms + tonumber(ARGV[1])
            local current = tonumber(redis.call("GET", KEYS[1]))
            if current ~= nil and current > deadline then
                deadline = current
            end
            local remaining = deadline - now_ms
            if remaining < 1 then
                redis.call("DEL", KEYS[1])
                return 0
            end
            redis.call("SET", KEYS[1], deadline, "PX", remaining)
            return remaining
            "#,
        );
        let remaining_ms: i64 = script
            .key(key)
            .arg(cooldown_ms)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(Duration::from_millis(remaining_ms.max(0) as u64))
    }

    async fn cooldown_remaining(&self, key: &str) -> Result<Duration> {
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local current = tonumber(redis.call("GET", KEYS[1]))
            if current == nil then
                return 0
            end
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            local remaining = current - now_ms
            if remaining < 0 then
                return 0
            end
            return remaining
            "#,
        );
        let remaining_ms: i64 = script
            .key(key)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(Duration::from_millis(remaining_ms.max(0) as u64))
    }

    async fn note_windowed_request(&self, key: &str, window: Duration) -> Result<u64> {
        let window_ms = ttl_millis(window)?;
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            local window_ms = tonumber(ARGV[1])
            local started = tonumber(redis.call("HGET", KEYS[1], "at"))
            if started == nil or (now_ms - started) >= window_ms then
                redis.call("HSET", KEYS[1], "at", now_ms, "requests", 0, "retries", 0)
            end
            local requests = redis.call("HINCRBY", KEYS[1], "requests", 1)
            redis.call("PEXPIRE", KEYS[1], window_ms * 2)
            return requests
            "#,
        );
        let requests: i64 = script
            .key(key)
            .arg(window_ms)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        requests.try_into().map_err(|_| {
            MoaError::StorageError("Redis returned a negative request count".to_string())
        })
    }

    async fn try_consume_retry_budget(
        &self,
        key: &str,
        window: Duration,
        budget_percent: u32,
        budget_floor: u64,
    ) -> Result<RetryBudgetDecision> {
        let window_ms = ttl_millis(window)?;
        let budget_floor: i64 = budget_floor.try_into().map_err(|_| {
            MoaError::ValidationError("retry budget floor is too large".to_string())
        })?;
        let mut connection = self.connection.clone();
        let script = redis::Script::new(
            r#"
            local clock = redis.call("TIME")
            local now_ms = (clock[1] * 1000) + math.floor(clock[2] / 1000)
            local window_ms = tonumber(ARGV[1])
            local percent = tonumber(ARGV[2])
            local floor_budget = tonumber(ARGV[3])
            local started = tonumber(redis.call("HGET", KEYS[1], "at"))
            if started == nil or (now_ms - started) >= window_ms then
                redis.call("HSET", KEYS[1], "at", now_ms, "requests", 0, "retries", 0)
            end
            local requests = tonumber(redis.call("HGET", KEYS[1], "requests")) or 0
            local retries = tonumber(redis.call("HGET", KEYS[1], "retries")) or 0
            local budget = math.floor((requests * percent) / 100)
            if budget < floor_budget then
                budget = floor_budget
            end
            local allowed = 0
            if retries < budget then
                retries = redis.call("HINCRBY", KEYS[1], "retries", 1)
                allowed = 1
            end
            redis.call("PEXPIRE", KEYS[1], window_ms * 2)
            return {allowed, requests, retries}
            "#,
        );
        let (allowed, requests, retries): (i64, i64, i64) = script
            .key(key)
            .arg(window_ms)
            .arg(i64::from(budget_percent))
            .arg(budget_floor)
            .invoke_async(&mut connection)
            .await
            .map_err(map_redis_error)?;
        Ok(RetryBudgetDecision {
            allowed: allowed == 1,
            requests: requests.max(0) as u64,
            retries: retries.max(0) as u64,
        })
    }
}

fn ttl_millis(ttl: Duration) -> Result<i64> {
    ttl.as_millis()
        .try_into()
        .map_err(|_| MoaError::ValidationError("runtime cache TTL is too large".to_string()))
}

fn map_redis_error(error: redis::RedisError) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::error::MoaError;
    use moa_core::traits::RuntimeCacheStore;

    use super::RedisRuntimeCacheStore;

    #[tokio::test]
    async fn redis_runtime_cache_rejects_unparseable_url_at_construction() {
        // Pins: a URL that fails `redis::Client::open` parsing is rejected up front
        // (before any async connect/PING), surfaced as a ConfigError. This covers only
        // the URL-parse branch, not the connect/PING failure path.
        let error = match RedisRuntimeCacheStore::new("not-a-redis-url").await {
            Ok(_) => panic!("unparseable Redis URL should fail runtime-cache construction"),
            Err(error) => error,
        };

        assert!(matches!(error, MoaError::ConfigError(_)));
    }

    /// Live Redis CAS/TTL coverage. Requires `MOA_RUN_LIVE_REDIS=1` plus a reachable
    /// Redis at `MOA_RUN_LIVE_REDIS_URL` (default `redis://127.0.0.1:6379`); the local
    /// compose stack exposes `valkey`. Pins the CAS and bounded-lease Lua scripts
    /// plus PX-millis TTL encoding so Memory-vs-Redis semantics cannot diverge.
    #[tokio::test]
    #[ignore = "requires a live Redis; set MOA_RUN_LIVE_REDIS=1"]
    async fn redis_runtime_cache_set_get_expire_and_cas_round_trip_docker() {
        // Accept common truthy values (1/true/yes/on) so a developer's `.env` enables it.
        let redis_enabled = std::env::var("MOA_RUN_LIVE_REDIS")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if !redis_enabled {
            panic!("MOA_RUN_LIVE_REDIS=1 is required to run the live Redis CAS test");
        }
        let url = std::env::var("MOA_RUN_LIVE_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

        let store = RedisRuntimeCacheStore::new(&url)
            .await
            .expect("connect to live Redis runtime cache");

        // Unique key per run so concurrent/leftover state cannot interfere.
        let key = format!("moa:test:cas:{}", uuid_like());
        let ttl = Duration::from_secs(30);

        // set -> get round-trips the stored bytes.
        store
            .set(&key, b"first".to_vec(), ttl)
            .await
            .expect("set value");
        assert_eq!(
            store.get(&key).await.expect("get value"),
            Some(b"first".to_vec())
        );

        // CAS expecting absent must fail when the key already holds a value.
        assert!(
            !store
                .compare_and_set(&key, None, b"absent-write".to_vec(), ttl)
                .await
                .expect("cas expecting absent"),
            "CAS expecting absent must not overwrite an existing key"
        );

        // CAS with a mismatched expected value must fail.
        assert!(
            !store
                .compare_and_set(&key, Some(b"wrong"), b"mismatch-write".to_vec(), ttl)
                .await
                .expect("cas with mismatch"),
            "CAS with a mismatched expected value must not overwrite"
        );
        assert_eq!(
            store.get(&key).await.expect("get after failed cas"),
            Some(b"first".to_vec()),
            "failed CAS attempts must leave the value untouched"
        );

        // CAS with the exact expected value must swap.
        assert!(
            store
                .compare_and_set(&key, Some(b"first"), b"second".to_vec(), ttl)
                .await
                .expect("cas with exact match"),
            "CAS with the exact expected value must swap"
        );
        assert_eq!(
            store.get(&key).await.expect("get after successful cas"),
            Some(b"second".to_vec())
        );

        // expire with a sub-millisecond-rounded short TTL eventually evicts the key.
        store
            .expire(&key, Duration::from_millis(50))
            .await
            .expect("expire key");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            store.get(&key).await.expect("get after expiry"),
            None,
            "key must be gone after its PX TTL elapses"
        );

        // CAS expecting absent must now succeed on the expired (absent) key.
        assert!(
            store
                .compare_and_set(&key, None, b"reborn".to_vec(), ttl)
                .await
                .expect("cas expecting absent after expiry"),
            "CAS expecting absent must succeed once the key is gone"
        );
        store.delete(&key).await.expect("cleanup key");

        let lease_key = format!("moa:test:leases:{{turn-admission}}:{}", uuid_like());
        assert!(
            store
                .try_acquire_bounded_lease(&lease_key, "session-a", 1, Duration::from_millis(100),)
                .await
                .expect("acquire first lease")
                .acquired
        );
        assert!(
            !store
                .try_acquire_bounded_lease(&lease_key, "session-b", 1, Duration::from_millis(100),)
                .await
                .expect("reject lease over limit")
                .acquired
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            store
                .try_acquire_bounded_lease(&lease_key, "session-b", 1, Duration::from_secs(1),)
                .await
                .expect("reclaim expired lease")
                .acquired
        );
        assert_eq!(
            store
                .release_bounded_lease(&lease_key, "session-b")
                .await
                .expect("release bounded lease"),
            0
        );
    }

    /// Live Redis coverage for the shared pacing, cooldown, and retry-budget Lua
    /// scripts, held to the same contract as the in-memory backend. Requires
    /// `MOA_RUN_LIVE_REDIS=1` plus a reachable Redis at `MOA_RUN_LIVE_REDIS_URL`.
    #[tokio::test]
    #[ignore = "requires a live Redis; set MOA_RUN_LIVE_REDIS=1"]
    async fn redis_backend_meets_the_shared_coordination_contract_docker() {
        let redis_enabled = std::env::var("MOA_RUN_LIVE_REDIS")
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        if !redis_enabled {
            panic!("MOA_RUN_LIVE_REDIS=1 is required to run the live Redis coordination test");
        }
        let url = std::env::var("MOA_RUN_LIVE_REDIS_URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        let store = RedisRuntimeCacheStore::new(&url)
            .await
            .expect("connect to live Redis runtime cache");

        crate::tests::assert_shared_coordination_conformance(
            &store,
            &format!("moa:test:coordination:{}", uuid_like()),
        )
        .await
        .expect("the Redis backend must satisfy the shared coordination contract");
    }

    /// Returns a process-and-time-unique suffix without pulling in a uuid dependency.
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{}-{nanos}", std::process::id())
    }
}
