//! Read-time cache for hybrid graph-memory retrieval.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, ScopeContext, ScopeTier, ScopedConn};
use moka::future::Cache;
use sqlx::PgPool;

use crate::planning::{PlannedQuery, Strategy};
use crate::retrieval::hybrid::{HybridRetriever, Result};
use crate::retrieval::{RetrievalHit, RetrievalRequest};

const DEFAULT_MAX_WORKSPACES: u64 = 1_000;
const DEFAULT_WORKSPACE_CAPACITY: u64 = 200;
const DEFAULT_TTL: Duration = Duration::from_secs(300);

type WorkspaceCache = Cache<CacheKey, CachedEntry>;

/// Backend abstraction used by the read-time cache.
#[async_trait]
pub trait RetrievalBackend: Send + Sync {
    /// Runs one uncached retrieval request.
    async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>>;
}

#[async_trait]
impl RetrievalBackend for HybridRetriever {
    async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>> {
        HybridRetriever::retrieve(self, req).await
    }
}

#[async_trait]
trait ChangelogVersionReader: Send + Sync {
    async fn current_version(&self, scope: &MemoryScope, workspace_id: &str) -> Result<i64>;
}

#[derive(Clone)]
struct PostgresChangelogVersionReader {
    pool: PgPool,
    assume_app_role: bool,
}

#[async_trait]
impl ChangelogVersionReader for PostgresChangelogVersionReader {
    async fn current_version(&self, scope: &MemoryScope, workspace_id: &str) -> Result<i64> {
        if uuid::Uuid::parse_str(workspace_id).is_err() {
            return Ok(0);
        }

        let scope_context = ScopeContext::from(scope.clone());
        let mut conn = ScopedConn::begin(&self.pool, &scope_context).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let version = sqlx::query_scalar::<_, i64>(
            "SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1",
        )
        .bind(workspace_id)
        .fetch_optional(conn.as_mut())
        .await?
        .unwrap_or(0);
        conn.commit().await?;
        Ok(version)
    }
}

/// Configuration for `CachedHybridRetriever`.
#[derive(Debug, Clone)]
pub struct CachedHybridRetrieverConfig {
    /// Maximum number of workspace caches retained by the outer LRU.
    pub max_workspaces: u64,
    /// Maximum retrieval entries retained per workspace.
    pub workspace_capacity: u64,
    /// Time-to-live for each cached retrieval entry.
    pub ttl: Duration,
    /// Whether user-scope queries may be cached.
    pub cache_user_scope: bool,
}

impl Default for CachedHybridRetrieverConfig {
    fn default() -> Self {
        Self {
            max_workspaces: DEFAULT_MAX_WORKSPACES,
            workspace_capacity: DEFAULT_WORKSPACE_CAPACITY,
            ttl: DEFAULT_TTL,
            cache_user_scope: false,
        }
    }
}

/// Per-workspace cache key for one planned retrieval.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct CacheKey {
    /// Workspace cache namespace. Global-only queries use `global`.
    pub workspace_id: String,
    /// Hash over canonical scope, plan, and retrieval parameters.
    pub fingerprint: [u8; 32],
}

/// Cached successful retrieval result and its write-version epoch.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    /// Retrieval hits returned by the inner retriever.
    pub hits: Vec<RetrievalHit>,
    /// `workspace_state.changelog_version` observed before retrieval.
    pub changelog_version: i64,
    /// Wall-clock time when this entry was inserted.
    pub cached_at: DateTime<Utc>,
}

/// Read-through LRU cache around `HybridRetriever`.
#[derive(Clone)]
pub struct CachedHybridRetriever {
    inner: Arc<dyn RetrievalBackend>,
    version_reader: Arc<dyn ChangelogVersionReader>,
    workspaces: Cache<String, WorkspaceCache>,
    config: CachedHybridRetrieverConfig,
}

impl CachedHybridRetriever {
    /// Creates a read-time cache around a production hybrid retriever.
    #[must_use]
    pub fn new(inner: Arc<HybridRetriever>, pool: PgPool) -> Self {
        Self::with_config(inner, pool, CachedHybridRetrieverConfig::default())
    }

    /// Creates a read-time cache with explicit configuration.
    #[must_use]
    pub fn with_config(
        inner: Arc<HybridRetriever>,
        pool: PgPool,
        config: CachedHybridRetrieverConfig,
    ) -> Self {
        Self::with_config_and_role(inner, pool, config, false)
    }

    /// Creates a read-time cache that assumes `moa_app` for owner-role integration tests.
    #[must_use]
    pub fn new_for_app_role(inner: Arc<HybridRetriever>, pool: PgPool) -> Self {
        Self::with_config_for_app_role(inner, pool, CachedHybridRetrieverConfig::default())
    }

    /// Creates an app-role read-time cache with explicit configuration.
    #[must_use]
    pub fn with_config_for_app_role(
        inner: Arc<HybridRetriever>,
        pool: PgPool,
        config: CachedHybridRetrieverConfig,
    ) -> Self {
        Self::with_config_and_role(inner, pool, config, true)
    }

    fn with_config_and_role(
        inner: Arc<HybridRetriever>,
        pool: PgPool,
        config: CachedHybridRetrieverConfig,
        assume_app_role: bool,
    ) -> Self {
        Self::with_parts(
            inner,
            Arc::new(PostgresChangelogVersionReader {
                pool,
                assume_app_role,
            }),
            config,
        )
    }

    fn with_parts(
        inner: Arc<dyn RetrievalBackend>,
        version_reader: Arc<dyn ChangelogVersionReader>,
        config: CachedHybridRetrieverConfig,
    ) -> Self {
        let workspaces = Cache::builder()
            .max_capacity(config.max_workspaces)
            .time_to_live(config.ttl)
            .build();
        Self {
            inner,
            version_reader,
            workspaces,
            config,
        }
    }

    /// Retrieves graph-memory hits, using the versioned cache when eligible.
    pub async fn retrieve(
        &self,
        planned: &PlannedQuery,
        req: RetrievalRequest,
    ) -> Result<Vec<RetrievalHit>> {
        let started = std::time::Instant::now();
        if !self.cacheable_scope(&req.scope) {
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "bypass").increment(1);
            return self.inner.retrieve(req).await;
        }

        let workspace_id = workspace_cache_id(&req.scope);
        let current_version = self
            .version_reader
            .current_version(&req.scope, &workspace_id)
            .await?;
        let key = CacheKey {
            workspace_id: workspace_id.clone(),
            fingerprint: fingerprint(planned, &req),
        };
        let workspace_cache = self.workspace_cache(&workspace_id).await;

        if let Some(entry) = workspace_cache.get(&key).await {
            if entry.changelog_version == current_version {
                metrics::histogram!("moa_retrieval_cache_hit_seconds")
                    .record(started.elapsed().as_secs_f64());
                metrics::counter!("moa_retrieval_cache_total", "outcome" => "hit").increment(1);
                metrics::gauge!("moa_retrieval_cache_entries")
                    .set(workspace_cache.entry_count() as f64);
                return Ok(entry.hits);
            }
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "stale").increment(1);
            workspace_cache.invalidate(&key).await;
        } else {
            metrics::counter!("moa_retrieval_cache_total", "outcome" => "miss").increment(1);
        }

        let hits = self.inner.retrieve(req).await?;
        workspace_cache
            .insert(
                key,
                CachedEntry {
                    hits: hits.clone(),
                    changelog_version: current_version,
                    cached_at: Utc::now(),
                },
            )
            .await;
        metrics::gauge!("moa_retrieval_cache_entries").set(workspace_cache.entry_count() as f64);
        Ok(hits)
    }

    async fn workspace_cache(&self, workspace_id: &str) -> WorkspaceCache {
        if let Some(cache) = self.workspaces.get(workspace_id).await {
            return cache;
        }

        let cache = Cache::builder()
            .max_capacity(self.config.workspace_capacity)
            .time_to_live(self.config.ttl)
            .build();
        self.workspaces
            .insert(workspace_id.to_string(), cache.clone())
            .await;
        cache
    }

    fn cacheable_scope(&self, scope: &MemoryScope) -> bool {
        !matches!(scope.tier(), ScopeTier::User) || self.config.cache_user_scope
    }
}

fn workspace_cache_id(scope: &MemoryScope) -> String {
    scope
        .workspace_id()
        .map(|workspace_id| workspace_id.to_string())
        .unwrap_or_else(|| "global".to_string())
}

fn fingerprint(planned: &PlannedQuery, req: &RetrievalRequest) -> [u8; 32] {
    *blake3::hash(canonicalize(planned, req).as_bytes()).as_bytes()
}

fn canonicalize(planned: &PlannedQuery, req: &RetrievalRequest) -> String {
    let mut out = String::new();
    out.push_str("strategy=");
    out.push_str(strategy_str(planned.strategy));
    out.push_str("|scope=");
    push_scope(&mut out, &req.scope);
    out.push_str("|layers=");
    for layer in &planned.scope_ancestors {
        push_scope(&mut out, layer);
        out.push(',');
    }
    out.push_str("|query_hash=");
    out.push_str(&blake3::hash(req.query_text.as_bytes()).to_hex());
    out.push_str("|seeds=");
    let mut seeds = planned.seeds.clone();
    seeds.sort_unstable();
    for seed in seeds {
        out.push_str(&seed.to_string());
        out.push(',');
    }
    out.push_str("|labels=");
    let mut labels = planned
        .label_hint
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
    out.push_str("|pii=");
    out.push_str(req.max_pii_class.as_str());
    out.push_str("|k=");
    out.push_str(&req.k_final.to_string());
    out.push_str("|rerank=");
    out.push_str(if req.use_reranker { "1" } else { "0" });
    out.push_str("|temporal=");
    if let Some(temporal) = planned.temporal_filter {
        out.push_str(&temporal.to_rfc3339());
    }
    out
}

fn push_scope(out: &mut String, scope: &MemoryScope) {
    match scope {
        MemoryScope::Global => out.push_str("global"),
        MemoryScope::Workspace { workspace_id } => {
            out.push_str("workspace:");
            out.push_str(workspace_id.as_str());
        }
        MemoryScope::User {
            workspace_id,
            user_id,
        } => {
            out.push_str("user:");
            out.push_str(workspace_id.as_str());
            out.push(':');
            out.push_str(user_id.as_str());
        }
    }
}

fn strategy_str(strategy: Strategy) -> &'static str {
    match strategy {
        Strategy::GraphFirst => "graph_first",
        Strategy::VectorFirst => "vector_first",
        Strategy::Both => "both",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

    use chrono::Utc;
    use moa_core::{MemoryScope, UserId, WorkspaceId};
    use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass};
    use uuid::Uuid;

    use super::*;
    use crate::retrieval::LegSources;

    #[tokio::test]
    async fn cache_hit_reuses_successful_workspace_retrieval() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(7));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(workspace_scope(), "auth service");
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
    async fn cache_invalidation_on_write_version_bump_misses() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version.clone(),
            CachedHybridRetrieverConfig::default(),
        );
        let planned = planned_query(workspace_scope(), "auth service");
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
    async fn user_scope_bypasses_cache_by_default() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend.clone(),
            version,
            CachedHybridRetrieverConfig::default(),
        );
        let scope = MemoryScope::User {
            workspace_id: WorkspaceId::new("workspace-a"),
            user_id: UserId::new("user-a"),
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
    async fn cache_eviction_at_capacity_does_not_crash() {
        let backend = Arc::new(CountingBackend::new());
        let version = Arc::new(MockVersionReader::new(1));
        let cache = CachedHybridRetriever::with_parts(
            backend,
            version,
            CachedHybridRetrieverConfig {
                workspace_capacity: 1,
                ..CachedHybridRetrieverConfig::default()
            },
        );
        let planned_a = planned_query(workspace_scope(), "auth service");
        let planned_b = planned_query(workspace_scope(), "deploy service");

        cache
            .retrieve(&planned_a, request(&planned_a, "what owns auth?"))
            .await
            .expect("first retrieval should succeed");
        cache
            .retrieve(&planned_b, request(&planned_b, "what owns deploy?"))
            .await
            .expect("second retrieval should succeed");
    }

    #[test]
    fn canonicalization_is_order_insensitive_for_seed_and_label_sets() {
        let seed_a = Uuid::now_v7();
        let seed_b = Uuid::now_v7();
        let mut left = planned_query(workspace_scope(), "auth service");
        left.seeds = vec![seed_b, seed_a];
        left.label_hint = Some(vec![NodeLabel::Fact, NodeLabel::Decision]);
        let mut right = left.clone();
        right.seeds = vec![seed_a, seed_b];
        right.label_hint = Some(vec![NodeLabel::Decision, NodeLabel::Fact]);

        assert_eq!(
            fingerprint(&left, &request(&left, "what owns auth?")),
            fingerprint(&right, &request(&right, "what owns auth?"))
        );
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
        async fn retrieve(&self, _req: RetrievalRequest) -> Result<Vec<RetrievalHit>> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let uid = Uuid::now_v7();
            Ok(vec![RetrievalHit {
                uid,
                score: call as f64,
                legs: LegSources {
                    graph: true,
                    vector: false,
                    lexical: false,
                },
                node: NodeIndexRow {
                    uid,
                    label: NodeLabel::Fact,
                    workspace_id: Some("workspace-a".to_string()),
                    user_id: None,
                    scope: "workspace".to_string(),
                    name: format!("hit {call}"),
                    pii_class: PiiClass::None,
                    valid_to: None,
                    valid_from: Utc::now(),
                    properties_summary: None,
                    last_accessed_at: Utc::now(),
                },
            }])
        }
    }

    struct MockVersionReader {
        version: AtomicI64,
    }

    impl MockVersionReader {
        fn new(version: i64) -> Self {
            Self {
                version: AtomicI64::new(version),
            }
        }

        fn set(&self, version: i64) {
            self.version.store(version, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl ChangelogVersionReader for MockVersionReader {
        async fn current_version(&self, _scope: &MemoryScope, _workspace_id: &str) -> Result<i64> {
            Ok(self.version.load(Ordering::SeqCst))
        }
    }

    fn workspace_scope() -> MemoryScope {
        MemoryScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-a"),
        }
    }

    fn planned_query(scope: MemoryScope, name: &str) -> PlannedQuery {
        let mut seed_bytes = [0_u8; 16];
        seed_bytes.copy_from_slice(&blake3::hash(name.as_bytes()).as_bytes()[..16]);
        PlannedQuery {
            strategy: Strategy::Both,
            seeds: vec![Uuid::from_bytes(seed_bytes)],
            label_hint: Some(vec![NodeLabel::Fact]),
            scope: scope.clone(),
            scope_ancestors: scope.ancestors(),
            temporal_filter: None,
        }
    }

    fn request(planned: &PlannedQuery, query: &str) -> RetrievalRequest {
        RetrievalRequest {
            seeds: planned.seeds.clone(),
            query_text: query.to_string(),
            query_embedding: vec![0.0; 1024],
            scope: planned.scope.clone(),
            label_filter: planned.label_hint.clone(),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: Some(planned.strategy),
        }
    }
}
