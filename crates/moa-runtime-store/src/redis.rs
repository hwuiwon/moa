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
    use moa_core::MoaError;

    use super::RedisRuntimeCacheStore;

    #[tokio::test]
    async fn redis_runtime_cache_rejects_invalid_url_before_startup() {
        // Pins: invalid Redis URLs fail during async runtime-cache construction.
        let error = match RedisRuntimeCacheStore::new("not-a-redis-url").await {
            Ok(_) => panic!("invalid Redis URL should fail runtime-cache construction"),
            Err(error) => error,
        };

        assert!(matches!(error, MoaError::ConfigError(_)));
    }
}
