# moa-runtime-store

Runtime cache store implementations behind `moa_core::traits::RuntimeCacheStore`,
used for ephemeral runtime coordination state. Backend selection fails closed
for distributed safety: `auto` resolves to Redis when a URL is configured, and
only selects the process-local memory backend with an explicit opt-in
(`runtime_cache.backend = "memory"` or `MOA_RUNTIME_CACHE_ALLOW_MEMORY=1`)
rather than silently degrading cross-instance coordination.

## Structure

- `lib.rs` — backend resolution: `select_runtime_cache_backend`,
  `ResolvedRuntimeCacheBackend`
- `memory.rs` — in-memory runtime cache store (`MemoryRuntimeCacheStore`)
- `redis.rs` — Redis runtime cache store (`RedisRuntimeCacheStore`, behind the
  `redis` feature)

## Features

- `redis` — enables the Redis backend and `RedisRuntimeCacheStore`
