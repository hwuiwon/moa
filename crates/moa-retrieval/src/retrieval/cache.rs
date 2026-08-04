//! Read-time cache for hybrid graph-memory retrieval.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use moa_memory_types::{MemoryScope, ScopeTier};
use moka::future::Cache;
use sqlx::PgPool;

use crate::planning::PlannedQuery;
use crate::retrieval::hybrid::HybridRetriever;
use crate::retrieval::types::Result;
use crate::retrieval::{RetrievalHit, RetrievalOutput, RetrievalRequest};

const DEFAULT_MAX_TENANTS: u64 = 1_000;
const DEFAULT_TENANT_CAPACITY: u64 = 200;
const DEFAULT_TTL: Duration = Duration::from_secs(300);

type TenantCache = Cache<CacheKey, CachedEntry>;

/// Backend abstraction used by the read-time cache.
#[async_trait]
pub trait RetrievalBackend: Send + Sync {
    /// Returns a stable fingerprint for the backend's ranking pipeline.
    fn ranking_fingerprint(&self) -> [u8; 32];

    /// Runs one uncached retrieval request, returning hits and provenance.
    async fn retrieve(&self, req: RetrievalRequest) -> Result<RetrievalOutput>;
}

/// Planned retrieval abstraction used by graph-memory pipeline tests and production caches.
#[async_trait]
pub trait PlannedRetriever: Send + Sync {
    /// Retrieves graph-memory hits and provenance for an already planned query.
    async fn retrieve(
        &self,
        planned: &PlannedQuery,
        req: RetrievalRequest,
    ) -> Result<RetrievalOutput>;

    /// Returns cached hits when a fresh cache entry already exists, without
    /// running the backend or requiring a query embedding.
    ///
    /// Callers use this to probe the cache before computing an embedding: a
    /// `Some` result means the full retrieval can be skipped, while `None`
    /// signals the caller to embed the query and call [`Self::retrieve`]. The
    /// default returns `None` so non-caching retrievers always fall back to the
    /// backend. The supplied `req` must carry every field that participates in
    /// the cache key (the query embedding does not), so an empty embedding is
    /// sufficient here.
    async fn retrieve_cached(
        &self,
        _planned: &PlannedQuery,
        _req: &RetrievalRequest,
    ) -> Result<Option<Vec<RetrievalHit>>> {
        Ok(None)
    }
}

#[async_trait]
impl RetrievalBackend for HybridRetriever {
    fn ranking_fingerprint(&self) -> [u8; 32] {
        HybridRetriever::ranking_fingerprint(self)
    }

    async fn retrieve(&self, req: RetrievalRequest) -> Result<RetrievalOutput> {
        HybridRetriever::retrieve_with_diagnostics(self, req).await
    }
}

#[async_trait]
trait ChangelogVersionReader: Send + Sync {
    async fn current_version(&self, scope: &MemoryScope) -> Result<i64>;
}

#[derive(Clone)]
struct PostgresChangelogVersionReader {
    pool: PgPool,
    assume_app_role: bool,
}

#[async_trait]
impl ChangelogVersionReader for PostgresChangelogVersionReader {
    async fn current_version(&self, scope: &MemoryScope) -> Result<i64> {
        let scope_context = RlsContext::from(scope.clone());
        let mut conn = ScopedConn::begin(&self.pool, &scope_context).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT changelog_version FROM moa.storage_partition_state WHERE tenant_id = $1",
        )
        .bind(scope.tenant_id().0)
        .fetch_optional(conn.as_mut())
        .await?
        .unwrap_or(0);
        conn.commit().await?;
        Ok(version)
    }
}

/// Configuration for `CachedHybridRetriever`.
#[derive(Debug, Clone)]
struct CachedHybridRetrieverConfig {
    /// Maximum number of tenant caches retained by the outer LRU.
    max_tenants: u64,
    /// Maximum retrieval entries retained per tenant.
    tenant_capacity: u64,
    /// Time-to-live for each cached retrieval entry.
    ttl: Duration,
}

impl Default for CachedHybridRetrieverConfig {
    fn default() -> Self {
        Self {
            max_tenants: DEFAULT_MAX_TENANTS,
            tenant_capacity: DEFAULT_TENANT_CAPACITY,
            ttl: DEFAULT_TTL,
        }
    }
}

/// Per-tenant cache key for one planned retrieval.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CacheKey {
    /// Tenant cache namespace.
    pub tenant_cache_id: String,
    /// Hash over canonical scope, plan, and retrieval parameters.
    pub fingerprint: [u8; 32],
}

/// Cached successful retrieval result and its write-version epoch.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    /// Retrieval hits returned by the inner retriever.
    pub hits: Vec<RetrievalHit>,
    /// `storage_partition_state.changelog_version` observed before retrieval.
    pub changelog_version: i64,
    /// Wall-clock time when this entry was inserted.
    pub cached_at: DateTime<Utc>,
}

/// Read-through LRU cache around `HybridRetriever`.
#[derive(Clone)]
pub struct CachedHybridRetriever {
    inner: Arc<dyn RetrievalBackend>,
    version_reader: Arc<dyn ChangelogVersionReader>,
    tenants: Cache<String, TenantCache>,
    config: CachedHybridRetrieverConfig,
}

impl CachedHybridRetriever {
    /// Creates a read-time cache around a production hybrid retriever.
    #[must_use]
    pub fn new(inner: Arc<HybridRetriever>, pool: PgPool) -> Self {
        Self::with_role(inner, pool, false)
    }

    /// Creates a read-time cache that assumes `moa_app` for owner-role integration tests.
    #[must_use]
    pub fn new_for_app_role(inner: Arc<HybridRetriever>, pool: PgPool) -> Self {
        Self::with_role(inner, pool, true)
    }

    fn with_role(inner: Arc<HybridRetriever>, pool: PgPool, assume_app_role: bool) -> Self {
        Self::with_parts(
            inner,
            Arc::new(PostgresChangelogVersionReader {
                pool,
                assume_app_role,
            }),
            CachedHybridRetrieverConfig::default(),
        )
    }

    fn with_parts(
        inner: Arc<dyn RetrievalBackend>,
        version_reader: Arc<dyn ChangelogVersionReader>,
        config: CachedHybridRetrieverConfig,
    ) -> Self {
        let tenants = Cache::builder()
            .max_capacity(config.max_tenants)
            .time_to_live(config.ttl)
            .build();
        Self {
            inner,
            version_reader,
            tenants,
            config,
        }
    }

    /// Retrieves graph-memory hits, using the versioned cache when eligible.
    ///
    /// A cache hit returns the stored hits with empty diagnostics and provenance:
    /// the backend did not run this turn, so there are no fresh stage timings,
    /// reranker scores, or graph paths to attribute.
    pub async fn retrieve(
        &self,
        planned: &PlannedQuery,
        req: RetrievalRequest,
    ) -> Result<RetrievalOutput> {
        let started = std::time::Instant::now();
        if !self.cacheable(&req) {
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "bypass").increment(1);
            return self.inner.retrieve(req).await;
        }

        let tenant_cache_id = tenant_cache_id(&req.scope);
        let current_version = self.version_reader.current_version(&req.scope).await?;
        let key = CacheKey {
            tenant_cache_id: tenant_cache_id.clone(),
            fingerprint: fingerprint(planned, &req, self.inner.ranking_fingerprint()),
        };
        let tenant_cache = self.tenant_cache(&tenant_cache_id).await;

        if let Some(entry) = tenant_cache.get(&key).await {
            if entry.changelog_version == current_version {
                metrics::histogram!("moa_retrieval_cache_hit_seconds")
                    .record(started.elapsed().as_secs_f64());
                metrics::counter!("moa_retrieval_cache_total", "outcome" => "hit").increment(1);
                return Ok(RetrievalOutput {
                    hits: entry.hits,
                    diagnostics: Default::default(),
                    provenance: Default::default(),
                });
            }
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "stale").increment(1);
            tenant_cache.invalidate(&key).await;
        } else {
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "miss").increment(1);
        }

        let output = self.inner.retrieve(req).await?;
        tenant_cache
            .insert(
                key,
                CachedEntry {
                    hits: output.hits.clone(),
                    changelog_version: current_version,
                    cached_at: Utc::now(),
                },
            )
            .await;
        Ok(output)
    }

    /// Returns a fresh cache entry without touching the backend or embedding.
    ///
    /// Used as a pre-embedding probe: the query embedding never participates in
    /// the cache key, so a caller can look up hits before paying for the
    /// embedding provider call and only embed when this returns `None`.
    pub async fn retrieve_cached(
        &self,
        planned: &PlannedQuery,
        req: &RetrievalRequest,
    ) -> Result<Option<Vec<RetrievalHit>>> {
        if !self.cacheable(req) {
            return Ok(None);
        }

        let tenant_cache_id = tenant_cache_id(&req.scope);
        let current_version = self.version_reader.current_version(&req.scope).await?;
        let key = CacheKey {
            tenant_cache_id: tenant_cache_id.clone(),
            fingerprint: fingerprint(planned, req, self.inner.ranking_fingerprint()),
        };
        let tenant_cache = self.tenant_cache(&tenant_cache_id).await;

        if let Some(entry) = tenant_cache.get(&key).await
            && entry.changelog_version == current_version
        {
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "hit").increment(1);
            return Ok(Some(entry.hits));
        }

        Ok(None)
    }

    async fn tenant_cache(&self, tenant_cache_id: &str) -> TenantCache {
        if let Some(cache) = self.tenants.get(tenant_cache_id).await {
            return cache;
        }

        let cache = Cache::builder()
            .max_capacity(self.config.tenant_capacity)
            .time_to_live(self.config.ttl)
            .build();
        self.tenants
            .insert(tenant_cache_id.to_string(), cache.clone())
            .await;
        cache
    }

    fn cacheable_scope(&self, scope: &MemoryScope) -> bool {
        !matches!(scope.tier(), ScopeTier::Contact)
    }

    /// Returns whether one request may participate in the cache at all.
    ///
    /// A request whose source-ACL context was never resolved has no epoch that a
    /// permission change could advance, so a stored entry for it would be
    /// unfalsifiable — it would keep serving after a revocation forever. Such a
    /// request bypasses the cache in both directions rather than being stored
    /// under a placeholder epoch.
    fn cacheable(&self, req: &RetrievalRequest) -> bool {
        self.cacheable_scope(&req.scope)
            && req.source_acl.acl_epoch() != moa_core::types::memory::SOURCE_ACL_EPOCH_UNRESOLVED
    }
}

#[async_trait]
impl PlannedRetriever for CachedHybridRetriever {
    async fn retrieve(
        &self,
        planned: &PlannedQuery,
        req: RetrievalRequest,
    ) -> Result<RetrievalOutput> {
        CachedHybridRetriever::retrieve(self, planned, req).await
    }

    async fn retrieve_cached(
        &self,
        planned: &PlannedQuery,
        req: &RetrievalRequest,
    ) -> Result<Option<Vec<RetrievalHit>>> {
        CachedHybridRetriever::retrieve_cached(self, planned, req).await
    }
}

fn tenant_cache_id(scope: &MemoryScope) -> String {
    scope.tenant_id().to_string()
}

fn fingerprint(
    planned: &PlannedQuery,
    req: &RetrievalRequest,
    ranking_fingerprint: [u8; 32],
) -> [u8; 32] {
    *blake3::hash(canonicalize(planned, req, ranking_fingerprint).as_bytes()).as_bytes()
}

fn canonicalize(
    planned: &PlannedQuery,
    req: &RetrievalRequest,
    ranking_fingerprint: [u8; 32],
) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("ranking=");
    for byte in ranking_fingerprint {
        write!(&mut out, "{byte:02x}").expect("write to string should not fail");
    }
    out.push('|');
    out.push_str("strategy=");
    out.push_str(planned.strategy.as_str());
    out.push_str("|scope=");
    push_scope(&mut out, &req.scope);
    out.push_str("|query_hash=");
    out.push_str(&blake3::hash(req.query_text.as_bytes()).to_hex());
    out.push_str("|seeds=");
    let mut seeds = planned.seeds.clone();
    seeds.sort_unstable();
    for seed in seeds {
        out.push_str(&seed.to_string());
        out.push(',');
    }
    // Both label dimensions are keyed from the request itself: `label_boost`
    // (soft ranking hint) and `label_filter` (hard leg filter) each change the
    // result, so the key must track the request field that drives ranking
    // rather than the plan's `label_hint` proxy it is derived from.
    out.push_str("|labels=");
    let mut labels = req
        .label_boost
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|label| label.as_str())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    for label in labels {
        out.push_str(label);
        out.push(',');
    }
    out.push_str("|filter_labels=");
    let mut filter_labels = req
        .label_filter
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|label| label.as_str())
        .collect::<Vec<_>>();
    filter_labels.sort_unstable();
    for label in filter_labels {
        out.push_str(label);
        out.push(',');
    }
    out.push_str("|disable_leg_timeouts=");
    out.push_str(if req.disable_leg_timeouts { "1" } else { "0" });
    out.push_str("|disable_graph_expansion=");
    out.push_str(if req.disable_graph_expansion {
        "1"
    } else {
        "0"
    });
    out.push_str("|pii=");
    out.push_str(req.max_pii_class.as_str());
    out.push_str("|authorization_policy_revision=");
    out.push_str(&blake3::hash(req.cleared_barriers.policy_revision().as_bytes()).to_hex());
    out.push_str("|cleared_barriers=");
    for barrier in &req.cleared_barriers {
        out.push_str(&barrier.as_str().len().to_string());
        out.push(':');
        out.push_str(barrier.as_str());
        out.push(',');
    }
    // Two dimensions, both required. The principal-set fingerprint separates
    // callers who may see different documents; the epoch separates the SAME
    // caller before and after a permission change. Dropping either one serves a
    // warm result across a boundary it must never cross — and only the already
    // opaque aggregate fingerprint appears here, never a principal.
    out.push_str("|source_acl_principals=");
    out.push_str(&req.source_acl.principal_set_fingerprint());
    out.push_str("|source_acl_epoch=");
    out.push_str(&req.source_acl.acl_epoch().to_string());
    out.push_str("|k=");
    out.push_str(&req.k_final.to_string());
    out.push_str("|rerank=");
    out.push_str(if req.use_reranker { "1" } else { "0" });
    out.push_str("|temporal=");
    if let Some(temporal) = req.as_of {
        out.push_str(&temporal.to_rfc3339());
    }
    out.push_str("|ranking_reference_time=");
    if let Some(reference_time) = req.ranking_reference_time {
        out.push_str(&reference_time.to_rfc3339());
    }
    out
}

fn push_scope(out: &mut String, scope: &MemoryScope) {
    match scope {
        MemoryScope::Tenant { tenant_id } => {
            out.push_str("tenant:");
            out.push_str(&tenant_id.to_string());
        }
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => {
            out.push_str("contact:");
            out.push_str(&tenant_id.to_string());
            out.push(':');
            out.push_str(&contact_id.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use chrono::Utc;
    use moa_core::types::security::SensitivityClass;
    use moa_core::{
        types::contact::ContactId, types::identifiers::TenantId,
        types::memory::InformationBarrierId,
    };
    use moa_memory_graph::{NodeIndexRow, NodeLabel};
    use moa_memory_types::MemoryScope;
    use uuid::Uuid;

    use super::*;
    use crate::planning::Strategy;
    use crate::retrieval::LegSources;
    use crate::retrieval::ranking::{
        RANKING_PIPELINE_VERSION, RankingConfig, ranking_fingerprint,
        ranking_fingerprint_for_version,
    };

    #[tokio::test]
    async fn cache_hit_reuses_successful_tenant_retrieval() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(7));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(tenant_scope(), "auth service");
        let req = request(&planned, "what owns auth?");

        let first = cache
            .retrieve(&planned, req.clone())
            .await
            .expect("cold retrieval should succeed");
        let second = cache
            .retrieve(&planned, req)
            .await
            .expect("warm retrieval should hit cache");

        assert_eq!(first, second);
        assert_eq!(backend.calls(), 1);
    }

    #[tokio::test]
    async fn cache_isolated_by_clearances_and_policy_revision() {
        // Pins: a cleared principal warming the shared tenant cache cannot
        // produce a hit for an uncleared principal, and a policy revision change
        // invalidates reuse even when the clearance set is unchanged.
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(7));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(tenant_scope(), "restricted deal");
        let mut cleared = request(&planned, "restricted deal");
        cleared.cleared_barriers =
            [InformationBarrierId::parse("deal-alpha").expect("valid barrier")]
                .into_iter()
                .collect();
        cleared.cleared_barriers = cleared.cleared_barriers.with_policy_revision("policy-v1");
        let mut uncleared = request(&planned, "restricted deal");
        uncleared.cleared_barriers = uncleared.cleared_barriers.with_policy_revision("policy-v1");
        let mut revised = cleared.clone();
        revised.cleared_barriers = revised.cleared_barriers.with_policy_revision("policy-v2");

        let cleared_output = cache
            .retrieve(&planned, cleared)
            .await
            .expect("cleared retrieval");
        let uncleared_output = cache
            .retrieve(&planned, uncleared)
            .await
            .expect("uncleared retrieval");
        cache
            .retrieve(&planned, revised)
            .await
            .expect("revised policy retrieval");

        assert_ne!(
            cleared_output.hits[0].uid, uncleared_output.hits[0].uid,
            "uncleared request must not receive the cleared cache entry"
        );
        assert_eq!(backend.calls(), 3);
    }

    #[tokio::test]
    async fn cache_invalidation_on_write_version_bump_misses() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version.clone(),
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(tenant_scope(), "auth service");
        let req = request(&planned, "what owns auth?");

        cache
            .retrieve(&planned, req.clone())
            .await
            .expect("cold retrieval should succeed");
        version.set(2);
        cache
            .retrieve(&planned, req)
            .await
            .expect("stale retrieval should refresh");

        assert_eq!(backend.calls(), 2);
    }

    #[tokio::test]
    async fn cache_hit_rechecks_changelog_version_for_exact_freshness() {
        // Pins: cache hits re-read the tenant changelog version so graph-memory
        // writes invalidate cached retrievals immediately.
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version.clone(),
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(tenant_scope(), "auth service");
        let req = request(&planned, "what owns auth?");

        cache
            .retrieve(&planned, req.clone())
            .await
            .expect("cold retrieval should succeed");
        cache
            .retrieve(&planned, req)
            .await
            .expect("warm retrieval should hit cache");

        assert_eq!(backend.calls(), 1, "second retrieval should be a cache hit");
        assert_eq!(
            version.reads(),
            2,
            "changelog version should be checked before each retrieval"
        );
    }

    #[tokio::test]
    async fn user_scope_bypasses_cache_by_default() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig::default(),
        );
        let scope = MemoryScope::Contact {
            tenant_id: test_tenant_id(),
            contact_id: test_contact_id(),
        };
        let planned = planned_query(scope, "auth service");
        let req = request(&planned, "what owns auth?");

        cache
            .retrieve(&planned, req.clone())
            .await
            .expect("first user retrieval should succeed");
        cache
            .retrieve(&planned, req)
            .await
            .expect("second user retrieval should bypass cache");

        assert_eq!(backend.calls(), 2);
    }

    #[tokio::test]
    async fn cache_eviction_at_capacity_forces_backend_re_miss() {
        // Pins: a per-tenant cache capped at one entry cannot serve two distinct
        // plans from cache at once; the evicted plan re-misses and re-calls the
        // backend instead of returning a stale hit.
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig {
                tenant_capacity: 1,
                ..CachedHybridRetrieverConfig::default()
            },
        );
        let planned_a = planned_query(tenant_scope(), "auth service");
        let planned_b = planned_query(tenant_scope(), "deploy service");

        // Warm plan A and confirm it is genuinely cached (a second read is free).
        cache
            .retrieve(&planned_a, request(&planned_a, "what owns auth?"))
            .await
            .expect("cold A retrieval should succeed");
        cache
            .retrieve(&planned_a, request(&planned_a, "what owns auth?"))
            .await
            .expect("warm A retrieval should hit cache");
        assert_eq!(backend.calls(), 1, "plan A should be cached after warming");

        // Insert plan B; capacity 1 means A and B cannot both remain cached.
        cache
            .retrieve(&planned_b, request(&planned_b, "what owns deploy?"))
            .await
            .expect("cold B retrieval should succeed");
        assert_eq!(backend.calls(), 2);

        // Force the per-tenant cache to apply capacity-based eviction.
        let tenant_cache = cache.tenant_cache(&tenant_cache_id(&tenant_scope())).await;
        tenant_cache.run_pending_tasks().await;
        assert!(
            tenant_cache.entry_count() <= 1,
            "a capacity-1 cache must retain at most one entry"
        );

        // Re-reading both distinct plans cannot be served entirely from a
        // single-entry cache, so at least one must re-miss and re-call the
        // backend (a non-evicting cache would keep both and add zero calls).
        cache
            .retrieve(&planned_a, request(&planned_a, "what owns auth?"))
            .await
            .expect("A re-read should succeed");
        cache
            .retrieve(&planned_b, request(&planned_b, "what owns deploy?"))
            .await
            .expect("B re-read should succeed");
        assert!(
            backend.calls() >= 3,
            "capacity eviction must force at least one backend re-call, got {}",
            backend.calls()
        );
    }

    #[test]
    fn canonicalization_is_order_insensitive_for_seed_and_label_sets() {
        let seed_a = Uuid::now_v7();
        let seed_b = Uuid::now_v7();
        let mut left = planned_query(tenant_scope(), "auth service");
        left.seeds = vec![seed_b, seed_a];
        left.label_hint = Some(vec![NodeLabel::Fact, NodeLabel::Decision]);
        let mut right = left.clone();
        right.seeds = vec![seed_a, seed_b];
        right.label_hint = Some(vec![NodeLabel::Decision, NodeLabel::Fact]);

        assert_eq!(
            fingerprint(
                &left,
                &request(&left, "what owns auth?"),
                ranking_fingerprint(&RankingConfig::default()),
            ),
            fingerprint(
                &right,
                &request(&right, "what owns auth?"),
                ranking_fingerprint(&RankingConfig::default()),
            )
        );
    }

    #[test]
    fn temporal_cache_fingerprint_uses_request_as_of() {
        // Pins: cache identity follows the executable retrieval request, not stale plan state.
        let planned = planned_query(tenant_scope(), "auth service");
        let current = request(&planned, "what owned auth?");
        let mut historical = current.clone();
        historical.as_of = Some(
            chrono::DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                .expect("test timestamp should parse")
                .with_timezone(&Utc),
        );

        assert_ne!(
            fingerprint(
                &planned,
                &current,
                ranking_fingerprint(&RankingConfig::default()),
            ),
            fingerprint(
                &planned,
                &historical,
                ranking_fingerprint(&RankingConfig::default()),
            )
        );
    }

    #[test]
    fn cache_key_separates_principal_sets_and_acl_epochs() {
        // Pins BOTH source-ACL cache dimensions. The principal-set fingerprint
        // separates two colleagues who may see different documents; the epoch
        // separates the SAME colleague before and after a permission change.
        // Dropping either one serves a warm result across a boundary it must
        // never cross, and the epoch is the only one that can catch a revocation
        // for an unchanged caller.
        let planned = planned_query(tenant_scope(), "auth service");
        let ranking = ranking_fingerprint(&RankingConfig::default());
        let alice = moa_core::types::memory::SourcePrincipalFingerprint::from_digest(1, [0xA1; 32]);
        let bob = moa_core::types::memory::SourcePrincipalFingerprint::from_digest(1, [0xB2; 32]);

        let mut alice_request = request(&planned, "what owns auth?");
        alice_request.source_acl =
            moa_core::types::memory::SourceAclContext::new([alice.clone()], 4);
        let mut bob_request = alice_request.clone();
        bob_request.source_acl = moa_core::types::memory::SourceAclContext::new([bob], 4);
        assert_ne!(
            fingerprint(&planned, &alice_request, ranking),
            fingerprint(&planned, &bob_request, ranking),
            "two callers with different principals must not share a cache entry"
        );

        let mut alice_after_revocation = alice_request.clone();
        alice_after_revocation.source_acl =
            moa_core::types::memory::SourceAclContext::new([alice], 5);
        assert_ne!(
            fingerprint(&planned, &alice_request, ranking),
            fingerprint(&planned, &alice_after_revocation, ranking),
            "the same caller must not be served a result computed before an ACL change"
        );

        // The aggregate fingerprint is the only ACL value that may appear, and it
        // is a digest of already-opaque values.
        let canonical = canonicalize(&planned, &alice_request, ranking);
        assert!(canonical.contains("source_acl_principals="));
        assert!(canonical.contains("source_acl_epoch=4"));
        assert!(
            !canonical.contains("a1a1a1"),
            "a raw principal fingerprint must not appear in the cache key: {canonical}"
        );
    }

    #[test]
    fn cache_key_changes_with_ranking_fingerprint() {
        // Pins: final ranked hits cannot be reused across ranking-weight changes.
        let planned = planned_query(tenant_scope(), "auth service");
        let req = request(&planned, "what owns auth?");
        let baseline = RankingConfig::default();
        let mut tuned = RankingConfig::default();
        tuned.weights.subject_match += 0.1;

        assert_ne!(
            fingerprint(&planned, &req, ranking_fingerprint(&baseline)),
            fingerprint(&planned, &req, ranking_fingerprint(&tuned))
        );
    }

    #[test]
    fn cache_key_changes_with_pipeline_version_bump() {
        // Pins: the ranking pipeline version participates in the cache key, so
        // bumping it invalidates every cached entry instead of silently serving
        // hits ranked by an older pipeline. Holds the config constant and varies
        // only the version to isolate the version's effect.
        let planned = planned_query(tenant_scope(), "auth service");
        let req = request(&planned, "what owns auth?");
        let config = RankingConfig::default();

        let current = ranking_fingerprint_for_version(RANKING_PIPELINE_VERSION, &config);
        let bumped = ranking_fingerprint_for_version(RANKING_PIPELINE_VERSION + 1, &config);
        assert_ne!(
            current, bumped,
            "ranking fingerprint must change when the pipeline version bumps"
        );

        assert_ne!(
            fingerprint(&planned, &req, current),
            fingerprint(&planned, &req, bumped),
            "cache key must diverge across a pipeline version bump"
        );
        // The production fingerprint is the current-version one.
        assert_eq!(ranking_fingerprint(&config), current);
    }

    #[derive(Default)]
    struct CountingBackend {
        calls: AtomicUsize,
    }

    impl CountingBackend {
        fn new() -> Self {
            Self::default()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RetrievalBackend for CountingBackend {
        fn ranking_fingerprint(&self) -> [u8; 32] {
            ranking_fingerprint(&RankingConfig::default())
        }

        async fn retrieve(&self, _req: RetrievalRequest) -> Result<RetrievalOutput> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let uid = Uuid::now_v7();
            Ok(RetrievalOutput {
                hits: vec![RetrievalHit {
                    uid,
                    score: call as f64,
                    legs: LegSources {
                        graph: true,
                        vector: false,
                        lexical: false,
                    },
                    similarity: None,
                    lexical_backend: None,
                    source_tier: crate::retrieval::SourceTier::UserMemory,
                    knowledge_chunk: None,
                    node: NodeIndexRow {
                        uid,
                        label: NodeLabel::Fact,
                        storage_partition_id: Some("tenant-a".to_string()),
                        contact_id: None,
                        scope: "tenant".to_string(),
                        name: format!("hit {call}"),
                        pii_class: SensitivityClass::None,
                        valid_to: None,
                        valid_from: Utc::now(),
                        properties_summary: None,
                        last_accessed_at: Utc::now(),
                        quality_score: 0.5,
                    },
                }],
                diagnostics: Default::default(),
                provenance: Default::default(),
            })
        }
    }

    struct MockVersionReader {
        version: AtomicI64,
        reads: AtomicUsize,
    }

    impl MockVersionReader {
        fn new(version: i64) -> Self {
            Self {
                version: AtomicI64::new(version),
                reads: AtomicUsize::new(0),
            }
        }

        fn set(&self, version: i64) {
            self.version.store(version, Ordering::SeqCst);
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ChangelogVersionReader for MockVersionReader {
        async fn current_version(&self, _scope: &MemoryScope) -> Result<i64> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.version.load(Ordering::SeqCst))
        }
    }

    fn tenant_scope() -> MemoryScope {
        MemoryScope::Tenant {
            tenant_id: test_tenant_id(),
        }
    }

    fn test_tenant_id() -> TenantId {
        TenantId::from(Uuid::from_u128(0x100))
    }

    fn test_contact_id() -> ContactId {
        ContactId(Uuid::from_u128(0x101))
    }

    fn planned_query(scope: MemoryScope, name: &str) -> PlannedQuery {
        let mut seed_bytes = [0_u8; 16];
        seed_bytes.copy_from_slice(&blake3::hash(name.as_bytes()).as_bytes()[..16]);
        PlannedQuery {
            strategy: Strategy::Both,
            seeds: vec![Uuid::from_bytes(seed_bytes)],
            label_hint: Some(vec![NodeLabel::Fact]),
            scope: scope.clone(),
            temporal_filter: None,
        }
    }

    fn request(planned: &PlannedQuery, query: &str) -> RetrievalRequest {
        RetrievalRequest {
            source_acl: moa_core::types::memory::SourceAclContext::empty(0),
            cleared_barriers: Default::default(),
            seeds: planned.seeds.clone(),
            query_text: query.to_string(),
            query_embedding: Some(
                moa_memory_vector::QueryEmbedding::new(vec![0.0; 1024], "test-model")
                    .expect("valid query embedding"),
            ),
            scope: planned.scope.clone(),
            // Mirror production: the planner-inferred hint rides as a soft
            // boost, and only a scope plan would set the hard `label_filter`.
            label_filter: None,
            label_boost: planned.label_hint.clone(),
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: Some(planned.strategy),
            as_of: planned.temporal_filter,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
        }
    }
}
