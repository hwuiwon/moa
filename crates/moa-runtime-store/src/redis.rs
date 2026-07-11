//! Redis runtime cache store.

use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
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
    /// compose stack exposes `valkey`. Pins the CAS Lua script and PX-millis TTL
    /// encoding so Memory-vs-Redis CAS semantics cannot silently diverge.
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
