//! Redis runtime cache store.

use std::time::Duration;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::RuntimeCacheStore;
use redis::AsyncCommands;

/// Redis-backed runtime cache used for shared coordination state.
#[derive(Clone)]
pub struct RedisRuntimeCacheStore {
    client: redis::Client,
}

impl RedisRuntimeCacheStore {
    /// Creates a Redis runtime cache from a Redis URL.
    pub fn new(redis_url: &str) -> Result<Self> {
        let client = redis::Client::open(redis_url)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        Ok(Self { client })
    }

    async fn connection(&self) -> Result<redis::aio::MultiplexedConnection> {
        self.client
            .get_multiplexed_async_connection()
            .await
            .map_err(map_redis_error)
    }
}

#[async_trait]
impl RuntimeCacheStore for RedisRuntimeCacheStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let mut connection = self.connection().await?;
        connection.get(key).await.map_err(map_redis_error)
    }

    async fn set(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<()> {
        let mut connection = self.connection().await?;
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
        let mut connection = self.connection().await?;
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
        let mut connection = self.connection().await?;
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
        let mut connection = self.connection().await?;
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
