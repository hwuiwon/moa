# Step M17 — Read-time cache (changelog-version invalidation)

_Add a per-workspace LRU cache keyed on `(workspace_id, scope_layers, query_hash, changelog_version)`. The cache is invalidated atomically by `workspace_state.changelog_version`, which already increments on every write (M06). Cache hit rate target ≥70% during typical agentic workloads._

## 1 What this step is about

Retrieval against the hybrid retriever (M15+M16) is the hottest path in the system. Most agentic workloads issue the same canonical sub-questions repeatedly within a session ("what's our CI setup?", "who owns auth?"). The cache key includes `changelog_version` for the workspace, so any write to that workspace's data invalidates relevant entries by version mismatch — no manual purge needed.

## 2 Files to read

- M06 `workspace_state.changelog_version` (we already increment it on every write)
- M15 `HybridRetriever::retrieve` (we wrap it)
- M16 `PlannedQuery` (input to cache key)

## 3 Goal

Cache wrapper around `HybridRetriever::retrieve` with:
- LRU (moka), 1000 entries default, 5min TTL.
- Cache key: `blake3(workspace_id || scope_canonical || planned_query_canonical)` + `changelog_version` epoch.
- Hit/miss/stale metrics.
- Per-workspace cache (not global) to avoid cross-tenant key collisions.

## 4 Rules

- **Cache only successful retrievals.** Errors are not cached.
- **Stale-on-version-bump**: if `current_changelog_version > cached_version`, treat as miss. The version is fetched before computing key, so cache is read-strict.
- **No cache for User-scope queries** (low reuse; high RAM cost). Configurable.
- **Cache size per workspace** is small (top 200 entries) so a busy workspace doesn't evict everyone else's hits.

## 5 Tasks

### 5a Migration (cosmetic)

Just confirms `workspace_state.changelog_version` exists from M06; no schema change needed in this step.

### 5b Wrapper struct

`crates/moa-brain/src/retrieval/cache.rs`:

```rust
use moka::future::Cache;

pub struct CachedHybridRetriever {
    inner: Arc<HybridRetriever>,
    pool: PgPool,
    cache: Cache<CacheKey, CachedEntry>,
}

#[derive(Clone, Hash, Eq, PartialEq)]
pub struct CacheKey {
    pub workspace_id: Uuid,
    pub fingerprint: [u8; 32],   // blake3 over canonical PlannedQuery + scope
}

#[derive(Clone)]
pub struct CachedEntry {
    pub hits: Vec<RetrievalHit>,
    pub changelog_version: i64,
    pub cached_at: DateTime<Utc>,
}

impl CachedHybridRetriever {
    pub fn new(inner: Arc<HybridRetriever>, pool: PgPool) -> Self {
        Self {
            inner, pool,
            cache: Cache::builder()
                .max_capacity(1000)
                .time_to_live(Duration::from_secs(300))
                .build(),
        }
    }

    pub async fn retrieve(&self, planned: PlannedQuery, req: RetrievalRequest, ws: Uuid)
        -> Result<Vec<RetrievalHit>>
    {
        // 1. Look up current version
        let cur_ver: Option<i64> = sqlx::query_scalar!(
            "SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1", ws
        ).fetch_optional(&self.pool).await?.flatten();
        let cur_ver = cur_ver.unwrap_or(0);

        // 2. Compute key
        let fingerprint = blake3::hash(canonicalize(&planned, &req.scope).as_bytes()).as_bytes().to_owned();
        let key = CacheKey { workspace_id: ws, fingerprint };

        // 3. Try cache
        if let Some(entry) = self.cache.get(&key).await {
            if entry.changelog_version == cur_ver {
                metrics::counter!("moa_retrieval_cache_total", "outcome"=>"hit").increment(1);
                return Ok(entry.hits.clone());
            }
            metrics::counter!("moa_retrieval_cache_total", "outcome"=>"stale").increment(1);
        } else {
            metrics::counter!("moa_retrieval_cache_total", "outcome"=>"miss").increment(1);
        }

        // 4. Run + insert
        let hits = self.inner.retrieve(req).await?;
        self.cache.insert(key, CachedEntry {
            hits: hits.clone(), changelog_version: cur_ver, cached_at: Utc::now(),
        }).await;
        Ok(hits)
    }
}
```

### 5c Canonicalization

```rust
fn canonicalize(planned: &PlannedQuery, scope: &MemoryScope) -> String {
    let mut s = format!("{}|{:?}|{}|", planned.strategy as u8, planned.label_hint, scope.tier_str());
    if let Some(w) = scope.workspace_id() { s.push_str(&w.to_string()); }
    s.push('|');
    if let Some(u) = scope.user_id() { s.push_str(&u.to_string()); }
    s.push('|');
    let mut seeds = planned.seeds.clone(); seeds.sort();
    for u in seeds { s.push_str(&u.to_string()); s.push(','); }
    s
}
```

### 5d Skip cache for User scope (configurable)

```rust
if matches!(scope.tier(), ScopeTier::User) && !self.cache_user_scope {
    return self.inner.retrieve(req).await;
}
```

### 5e Wire into pipeline

`pipeline/memory_retriever.rs::retrieve_for_query` wraps via the cached retriever. No changes to caller code beyond construction.

## 6 Deliverables

- `crates/moa-brain/src/retrieval/cache.rs` (~250 lines).
- Updated `pipeline/memory_retriever.rs`.
- Metrics emit (cache hit/miss/stale counters + size gauge).

## 7 Acceptance criteria

1. Cold query hits storage; warm same query hits cache (latency drops by >5×).
2. After a fast_remember on the workspace, identical cached query returns fresh result (version mismatch → miss).
3. Cache eviction at capacity does not crash.
4. P95 latency for cache-hit path <5ms.
5. Hit rate ≥70% under M30 workload.

## 8 Tests

```sh
cargo test -p moa-brain cache_hit
cargo test -p moa-brain cache_invalidation_on_write
cargo bench -p moa-brain cache_p95
```

## 9 Cleanup

- Remove any ad-hoc memo caches in retrieval-related code.
- Remove the previous "warm wiki" cache loader (no analog post-M28).

## 10 What's next

**M18 — Skills migration to Postgres rows.**
