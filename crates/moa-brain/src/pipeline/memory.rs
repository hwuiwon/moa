//! Stage 7: graph memory retrieval and prompt injection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::try_join_all;
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError, error::Result, traits::ContextProcessor, traits::EmbeddingProvider,
    traits::LineageHandle, traits::NullLineageHandle, traits::StageApply,
    types::context::ContextMessage, types::context::ContextSourceRef, types::context::ExcludedItem,
    types::context::ProcessorOutput, types::context::WorkingContext, types::identifiers::SessionId,
    types::query_rewrite::QueryRewriteResult,
};
use moa_memory_graph::{GraphStore, PiiClass, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::VectorStoreFactory;
use sqlx::PgPool;

use crate::planning::{PlannedQuery, Strategy};
use crate::retrieval::{
    MemoryAdmissionPolicy, PlannedRetriever, RetrievalHit, RetrievalRequest, RetrievalScopePlan,
    RetrievalStrategy, decompose_query, dedupe_and_rank_hits, route_query,
};

/// Maximum number of scope-keyed retrieval runtimes retained process-wide.
const SCOPED_RUNTIME_CACHE_CAPACITY: u64 = 512;
/// Idle lifetime for a cached scope retrieval runtime before it is rebuilt.
const SCOPED_RUNTIME_CACHE_TTL: Duration = Duration::from_secs(300);

/// Builds the capacity- and time-bounded cache of per-scope retrieval runtimes.
fn build_scoped_runtime_cache() -> moka::future::Cache<MemoryScope, Arc<ScopedRetrievalRuntime>> {
    moka::future::Cache::builder()
        .max_capacity(SCOPED_RUNTIME_CACHE_CAPACITY)
        .time_to_live(SCOPED_RUNTIME_CACHE_TTL)
        .build()
}

mod lineage;
mod rendering;

use lineage::lineage_context_from_context;
use rendering::render_memory_context_with_budget;

const MEMORY_BUDGET_DIVISOR: usize = 5;
const GRAPH_MEMORY_RESULTS: usize = 4;
const MAX_MEMORY_EVIDENCE_RANKED_DEPTH: usize = 50;
const MIN_PAGE_EXCERPT_TOKENS: usize = 96;
pub(crate) const MEMORY_REMINDER_PREFIX: &str = "<memory-reminder>";

/// Working-context metadata key recording whether the read-only agentic memory
/// tools (`memory_search`/`memory_navigate`) should be surfaced onto this turn.
///
/// The memory stage owns the decision because it is the only stage that knows
/// both the routed strategy and whether injected retrieval returned anything.
/// The harness reads this flag after compilation and, when it is `true`, appends
/// the gated tool schemas to the request (plan Task 11).
pub const OFFER_RETRIEVAL_TOOLS_METADATA_KEY: &str = "_moa.memory.offer_retrieval_tools";

/// Explicit request for admitted, prompt-ready graph-memory evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEvidenceRequest {
    /// Natural-language query routed through the production memory planner.
    pub query: String,
    /// Maximum estimated tokens in the returned rendered evidence.
    pub evidence_token_budget: usize,
    /// Maximum admitted graph hits retained before evidence rendering.
    ranked_occurrence_depth: usize,
}

impl MemoryEvidenceRequest {
    /// Creates a memory-evidence request with one explicit token budget.
    #[must_use]
    pub fn new(query: impl Into<String>, evidence_token_budget: usize) -> Self {
        Self {
            query: query.into(),
            evidence_token_budget,
            ranked_occurrence_depth: GRAPH_MEMORY_RESULTS,
        }
    }

    /// Overrides the admitted ranked-hit depth for evaluation retrieval.
    pub fn with_ranked_occurrence_depth(mut self, depth: usize) -> Result<Self> {
        if !(1..=MAX_MEMORY_EVIDENCE_RANKED_DEPTH).contains(&depth) {
            return Err(MoaError::ValidationError(
                "memory evidence ranked occurrence depth must be in 1..=50".to_string(),
            ));
        }
        self.ranked_occurrence_depth = depth;
        Ok(self)
    }

    /// Returns the admitted ranked-hit depth requested before rendering.
    #[must_use]
    pub const fn ranked_occurrence_depth(&self) -> usize {
        self.ranked_occurrence_depth
    }
}

/// Typed source metadata projected from one admitted graph-memory hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEvidenceSourceMetadata {
    /// Stable graph node identifier for the rendered hit.
    pub graph_uid: uuid::Uuid,
    /// Finalized session that produced this memory, when recorded by ingestion.
    pub source_session_id: Option<SessionId>,
    /// Finalized turn sequence that produced this memory, when recorded by ingestion.
    pub source_turn_seq: Option<u64>,
}

/// Admitted graph-memory evidence rendered exactly as stage 7 renders it.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEvidenceResponse {
    /// Ranked admitted hits before token-budgeted evidence rendering.
    pub hits: Vec<RetrievalHit>,
    /// Exact `<knowledge_context>` payload produced by the stage-7 renderer.
    pub rendered_evidence: String,
    /// Estimated tokens consumed by `rendered_evidence`.
    pub consumed_evidence_tokens: usize,
    /// Citation refs for the rendered ranked-hit prefix only.
    pub source_refs: Vec<ContextSourceRef>,
    /// Typed ingestion provenance aligned one-to-one with all ranked `hits`.
    pub source_metadata: Vec<MemoryEvidenceSourceMetadata>,
}

impl MemoryEvidenceResponse {
    fn empty() -> Self {
        Self {
            hits: Vec::new(),
            rendered_evidence: String::new(),
            consumed_evidence_tokens: 0,
            source_refs: Vec::new(),
            source_metadata: Vec::new(),
        }
    }
}

/// Decides whether the agentic memory tools are offered on a turn.
///
/// They are offered when the router selected the agentic strategy, or as a
/// cheap CRAG-style "insufficient context" fallback when a retrieval-intent turn
/// injected no hits. Chit-chat (`Skip`) never offers them.
fn should_offer_retrieval_tools(strategy: RetrievalStrategy, hits_empty: bool) -> bool {
    match strategy {
        RetrievalStrategy::Agentic => true,
        RetrievalStrategy::Skip => false,
        RetrievalStrategy::Fast | RetrievalStrategy::Deep => hits_empty,
    }
}

/// One planned scope together with the outcome of its pre-embedding cache probe.
struct ScopeProbe<'a> {
    scope_plan: &'a RetrievalScopePlan,
    planned: PlannedQuery,
    runtime: Arc<ScopedRetrievalRuntime>,
    cached_hits: Option<Vec<RetrievalHit>>,
}

/// Injects graph-memory retrieval hits into the active turn context.
pub struct GraphMemoryRetriever {
    pool: PgPool,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    config: moa_core::config::MoaConfig,
    assume_app_role: bool,
    lineage: Arc<dyn LineageHandle>,
    result_limit: usize,
    planner: crate::planning::QueryPlanner,
    scoped_runtimes: moka::future::Cache<MemoryScope, Arc<ScopedRetrievalRuntime>>,
    runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
}

/// Shared graph-memory retrieval stage backed by a process-wide retriever.
#[derive(Clone)]
pub struct SharedGraphMemoryRetriever {
    inner: Arc<GraphMemoryRetriever>,
}

impl SharedGraphMemoryRetriever {
    /// Creates a shared graph-memory processor from a process-wide retriever.
    #[must_use]
    pub fn new(inner: Arc<GraphMemoryRetriever>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl ContextProcessor for SharedGraphMemoryRetriever {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn stage(&self) -> u8 {
        self.inner.stage()
    }

    fn parallelizable(&self) -> bool {
        self.inner.parallelizable()
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        self.inner.process(ctx).await
    }

    async fn fetch(&self, ctx: &WorkingContext) -> Result<Option<StageApply>> {
        self.inner.fetch(ctx).await
    }
}

/// Runtime backends for one graph-memory scope.
pub struct ScopedRetrievalRuntime {
    graph: Arc<dyn GraphStore>,
    hybrid: Arc<dyn PlannedRetriever>,
}

impl ScopedRetrievalRuntime {
    /// Creates a scoped runtime from graph planning and retrieval backends.
    #[must_use]
    pub fn new(graph: Arc<dyn GraphStore>, hybrid: Arc<dyn PlannedRetriever>) -> Self {
        Self { graph, hybrid }
    }
}

/// Factory for building graph-memory retrieval runtimes for individual scopes.
#[async_trait]
pub trait ScopedRetrievalRuntimeFactory: Send + Sync {
    /// Builds graph-memory backends for one memory scope.
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &moa_core::config::MoaConfig,
        pool: &PgPool,
        assume_app_role: bool,
    ) -> Result<ScopedRetrievalRuntime>;
}

struct PostgresScopedRetrievalRuntimeFactory {
    /// Shared bounded enrichment worker handle, cloned into each scoped
    /// [`HybridRetriever`]. `None` when constructed outside a Tokio runtime.
    enrichment: Option<crate::retrieval::enrichment::EnrichmentHandle>,
}

#[async_trait]
impl ScopedRetrievalRuntimeFactory for PostgresScopedRetrievalRuntimeFactory {
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &moa_core::config::MoaConfig,
        pool: &PgPool,
        assume_app_role: bool,
    ) -> Result<ScopedRetrievalRuntime> {
        let scope_context = RlsContext::from(scope.clone());
        let vector_factory = VectorStoreFactory::from_config(config);
        let pgvector_source = if assume_app_role {
            vector_factory.pgvector_source_for_app_role(pool.clone(), scope_context.clone())
        } else {
            vector_factory.pgvector_source(pool.clone(), scope_context.clone())
        };
        let graph_store = if assume_app_role {
            PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        } else {
            PostgresGraphStore::scoped(pool.clone(), scope_context)
        };
        let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
        let hybrid = Arc::new(
            crate::retrieval::HybridRetriever::from_config(
                config,
                pool.clone(),
                graph.clone(),
                pgvector_source,
            )
            .with_assume_app_role(assume_app_role)
            .with_enrichment(self.enrichment.clone()),
        );
        let cached: Arc<dyn PlannedRetriever> = if assume_app_role {
            Arc::new(crate::retrieval::CachedHybridRetriever::new_for_app_role(
                hybrid,
                pool.clone(),
            ))
        } else {
            Arc::new(crate::retrieval::CachedHybridRetriever::new(
                hybrid,
                pool.clone(),
            ))
        };
        Ok(ScopedRetrievalRuntime::new(graph, cached))
    }
}

impl GraphMemoryRetriever {
    /// Creates a graph-memory retriever backed by the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self::new_with_config(moa_core::config::MoaConfig::default(), pool, embedder)
    }

    /// Creates a graph-memory retriever backed by the shared Postgres pool and runtime config.
    #[must_use]
    pub fn new_with_config(
        config: moa_core::config::MoaConfig,
        pool: PgPool,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        // F20: spawn the single shared bounded enrichment worker when a Tokio
        // runtime is available (production and async tests). Sync callers
        // (eval/unit setup) get no worker and skip best-effort enrichment.
        let enrichment = tokio::runtime::Handle::try_current()
            .ok()
            .map(|_| crate::retrieval::enrichment::spawn_enrichment_worker(pool.clone()));
        Self {
            pool,
            embedder,
            config,
            assume_app_role: false,
            lineage: Arc::new(NullLineageHandle),
            result_limit: GRAPH_MEMORY_RESULTS,
            planner: crate::planning::QueryPlanner::new(),
            scoped_runtimes: build_scoped_runtime_cache(),
            runtime_factory: Arc::new(PostgresScopedRetrievalRuntimeFactory { enrichment }),
        }
    }

    /// Configures owner-role tests to assume the production app role during scoped reads.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Attaches the lineage sink used to capture retrieval traces.
    #[must_use]
    pub fn with_lineage(mut self, lineage: Arc<dyn LineageHandle>) -> Self {
        self.lineage = lineage;
        self
    }

    /// Overrides the number of final graph-memory hits injected into context.
    #[must_use]
    pub fn with_result_limit(mut self, result_limit: usize) -> Self {
        self.result_limit = result_limit;
        self
    }

    /// Overrides the scoped runtime factory used to build retrieval backends.
    #[must_use]
    pub fn with_scoped_runtime_factory(
        mut self,
        runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
    ) -> Self {
        self.runtime_factory = runtime_factory;
        self.scoped_runtimes = build_scoped_runtime_cache();
        self
    }

    /// Returns whether this retriever can run the vector leg.
    #[must_use]
    pub fn has_vector_retrieval(&self) -> bool {
        self.embedder.is_some()
    }

    /// Retrieves admitted graph-memory evidence through the production stage-7 path.
    ///
    /// The request is routed, planned, retrieved, admitted, fused, ranked, and
    /// rendered by the same implementation used for prompt injection. The
    /// Returned hits and source metadata are aligned in rank order before
    /// rendering; source refs cover only the independently budgeted rendered
    /// prefix. `consumed_evidence_tokens` never exceeds the request's positive
    /// explicit evidence-token budget.
    pub async fn retrieve_evidence(
        &self,
        ctx: &WorkingContext,
        request: MemoryEvidenceRequest,
    ) -> Result<MemoryEvidenceResponse> {
        if request.evidence_token_budget == 0 {
            return Err(MoaError::ValidationError(
                "memory evidence token budget must be positive".to_string(),
            ));
        }
        let query = request.query.trim();
        if query.is_empty() {
            return Err(MoaError::ValidationError(
                "memory evidence query must not be empty".to_string(),
            ));
        }

        let policy = MemoryAdmissionPolicy::from_working_context(ctx)?;
        if !policy.is_enabled() {
            return Ok(MemoryEvidenceResponse::empty());
        }
        let result_limit = policy
            .result_limit(request.ranked_occurrence_depth)
            .min(request.ranked_occurrence_depth);
        let (_, hits) = self
            .retrieve_admitted_hits(ctx, query, &policy, result_limit)
            .await?;
        if hits.is_empty() {
            return Ok(MemoryEvidenceResponse::empty());
        }

        let budgeted = render_memory_context_with_budget(&hits, request.evidence_token_budget);
        let source_metadata = hits
            .iter()
            .map(memory_evidence_source_metadata)
            .collect::<Result<Vec<_>>>()?;
        debug_assert_eq!(budgeted.hit_count, budgeted.rendered.source_refs.len());
        debug_assert!(budgeted.consumed_tokens <= request.evidence_token_budget);

        Ok(MemoryEvidenceResponse {
            hits,
            rendered_evidence: budgeted.rendered.section,
            consumed_evidence_tokens: budgeted.consumed_tokens,
            source_refs: budgeted.rendered.source_refs,
            source_metadata,
        })
    }

    /// Runs the production router and returns ranked hits after shared admission.
    async fn retrieve_admitted_hits(
        &self,
        ctx: &WorkingContext,
        query: &str,
        policy: &MemoryAdmissionPolicy,
        result_limit: usize,
    ) -> Result<(RetrievalStrategy, Vec<RetrievalHit>)> {
        let strategy = route_query(query);
        if strategy == RetrievalStrategy::Skip {
            return Ok((strategy, Vec::new()));
        }

        let retrieval_started = Instant::now();
        let hits = match strategy {
            RetrievalStrategy::Deep => {
                self.retrieve_hits_deep(ctx, query, policy, result_limit)
                    .await?
            }
            RetrievalStrategy::Fast | RetrievalStrategy::Agentic => {
                self.retrieve_hits(ctx, query.to_string(), policy, result_limit)
                    .await?
            }
            RetrievalStrategy::Skip => Vec::new(),
        };
        lineage::emit_retrieval_lineage(
            self.lineage.as_ref(),
            ctx,
            query,
            &hits,
            retrieval_started.elapsed(),
        );
        Ok((strategy, hits))
    }

    async fn retrieve_hits(
        &self,
        ctx: &WorkingContext,
        query: String,
        policy: &MemoryAdmissionPolicy,
        requested_result_limit: usize,
    ) -> Result<Vec<RetrievalHit>> {
        let retrieval_plan = policy.plans();
        if retrieval_plan.is_empty() {
            return Ok(Vec::new());
        }
        let result_limit = requested_result_limit;
        let max_pii_class = policy.max_pii_class()?;
        let query_str = query.as_str();

        // Plan every scope and probe its read-time cache in parallel, before
        // paying for a query embedding. The embedding is not part of the cache
        // key, so a probe can hit without one.
        let probes = try_join_all(retrieval_plan.iter().map(|scope_plan| {
            self.probe_scope(ctx, query_str, scope_plan, max_pii_class, result_limit)
        }))
        .await?;

        // Embed once, and only when at least one scope missed the cache.
        let needs_backend = probes.iter().any(|probe| probe.cached_hits.is_none());
        let query_embedding = if needs_backend {
            match self.embedder.as_deref() {
                // A query-embedding failure must degrade to lexical-only
                // retrieval rather than abort the turn: an empty embedding makes
                // the vector leg return nothing while the lexical leg still runs.
                Some(embedder) => match embed_query(embedder, query_str).await {
                    Ok(embedding) => embedding,
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "query embedding failed; degrading to lexical-only retrieval"
                        );
                        metrics::counter!(
                            "moa_retrieval_leg_degraded_total",
                            "leg" => "embedding",
                            "reason" => "error",
                        )
                        .increment(1);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };

        // Resolve the missed scopes against the backend in parallel; cached
        // scopes reuse their probe result.
        let query_embedding = query_embedding.as_slice();
        let scope_hits = try_join_all(probes.iter().map(|probe| async move {
            match &probe.cached_hits {
                Some(hits) => Ok::<_, MoaError>(hits.clone()),
                None => {
                    self.retrieve_scope_backend(
                        ctx,
                        query_str,
                        probe,
                        query_embedding,
                        max_pii_class,
                        result_limit,
                    )
                    .await
                }
            }
        }))
        .await?;

        // Merge in scope order, admitting each scope's hits under its own plan.
        let mut hits = Vec::new();
        for (probe, results) in probes.iter().zip(scope_hits) {
            hits.extend(
                results
                    .into_iter()
                    .filter_map(|hit| policy.admit_hit(hit, probe.scope_plan)),
            );
        }
        Ok(dedupe_and_rank_hits(hits, result_limit))
    }

    /// Runs the deep, decomposed retrieval path for multi-hop queries.
    ///
    /// The query is split into at most two self-contained sub-queries; each is
    /// retrieved through the same per-scope machinery as [`Self::retrieve_hits`],
    /// and the union of sub-query hits is fused through `dedupe_and_rank_hits`
    /// (dedup by uid keeping the max score) — the existing rank path, never a new
    /// fusion path. When decomposition cannot split the query it degrades to a
    /// single retrieval over the full query.
    async fn retrieve_hits_deep(
        &self,
        ctx: &WorkingContext,
        query: &str,
        policy: &MemoryAdmissionPolicy,
        requested_result_limit: usize,
    ) -> Result<Vec<RetrievalHit>> {
        let mut sub_queries = decompose_query(query);
        if sub_queries.is_empty() {
            sub_queries.push(query.to_string());
        }

        let result_limit = requested_result_limit;

        // F23: run the (≤2) decomposed sub-queries concurrently instead of
        // serially. `try_join_all` preserves the decomposition input order in its
        // output, so fusion stays deterministic before `dedupe_and_rank_hits`.
        let sub_results =
            try_join_all(sub_queries.into_iter().map(|sub_query| {
                self.retrieve_hits(ctx, sub_query, policy, requested_result_limit)
            }))
            .await?;
        let mut fused = Vec::new();
        for hits in sub_results {
            fused.extend(hits);
        }
        Ok(dedupe_and_rank_hits(fused, result_limit))
    }

    /// Plans one scope and probes its read-time cache without an embedding.
    async fn probe_scope<'a>(
        &self,
        ctx: &WorkingContext,
        query: &str,
        scope_plan: &'a RetrievalScopePlan,
        max_pii_class: PiiClass,
        result_limit: usize,
    ) -> Result<ScopeProbe<'a>> {
        let runtime = self.runtime_for_scope(scope_plan.scope()).await?;
        let planning =
            crate::planning::PlanningCtx::new(scope_plan.scope().clone(), runtime.graph.clone());
        let planned = self.planner.plan(query, &planning).await.map_err(|error| {
            MoaError::StorageError(format!("graph memory planning failed: {error}"))
        })?;
        // The probe request omits the embedding and lineage — neither takes part
        // in the cache key, so this fingerprints identically to the backend run.
        let probe_request = self.build_scope_request(
            ctx,
            query,
            Vec::new(),
            scope_plan,
            &planned,
            max_pii_class,
            result_limit,
            false,
        );
        let cached_hits = runtime
            .hybrid
            .retrieve_cached(&planned, &probe_request)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("graph memory retrieval failed: {error}"))
            })?;
        Ok(ScopeProbe {
            scope_plan,
            planned,
            runtime,
            cached_hits,
        })
    }

    /// Runs the backend retrieval for a scope that missed the read-time cache.
    async fn retrieve_scope_backend(
        &self,
        ctx: &WorkingContext,
        query: &str,
        probe: &ScopeProbe<'_>,
        query_embedding: &[f32],
        max_pii_class: PiiClass,
        result_limit: usize,
    ) -> Result<Vec<RetrievalHit>> {
        let request = self.build_scope_request(
            ctx,
            query,
            query_embedding.to_vec(),
            probe.scope_plan,
            &probe.planned,
            max_pii_class,
            result_limit,
            true,
        );
        probe
            .runtime
            .hybrid
            .retrieve(&probe.planned, request)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("graph memory retrieval failed: {error}"))
            })
    }

    /// Builds a scope retrieval request from a planned query and scope plan.
    #[allow(clippy::too_many_arguments)]
    fn build_scope_request(
        &self,
        ctx: &WorkingContext,
        query: &str,
        query_embedding: Vec<f32>,
        scope_plan: &RetrievalScopePlan,
        planned: &PlannedQuery,
        max_pii_class: PiiClass,
        result_limit: usize,
        emit_storage_lineage: bool,
    ) -> RetrievalRequest {
        let mut request = planned.clone().into_retrieval_request(
            query.to_string(),
            query_embedding,
            max_pii_class,
            result_limit,
            true,
        );
        // The memory evidence window is calibrated for the small injected
        // block, so this stage rides its request-scoped window policy from the
        // configured memory ranking knobs. Knowledge-lane retrievals keep the
        // default (off) policy and size their own top-k window.
        let ranking = &self.config.memory.retrieval.ranking;
        request.window_policy = crate::retrieval::EvidenceWindowPolicy {
            rerank_window: ranking.rerank_window,
            abstain_below_window_evidence: ranking.abstain_below_window_evidence,
        };
        if let Some(label_filter) = scope_plan.label_filter() {
            request.label_filter = Some(label_filter.to_vec());
        }
        request.disable_graph_expansion = should_disable_graph_expansion(scope_plan);
        if matches!(
            scope_plan.source_tier(),
            crate::retrieval::SourceTier::TenantKnowledge
        ) {
            request.strategy = Some(Strategy::VectorFirst);
        }
        if emit_storage_lineage {
            request.lineage = Some(lineage_context_from_context(ctx));
        }
        request
    }

    /// Returns a per-scope retrieval runtime, reusing one from the bounded cache
    /// when present. All scope tiers (tenant and contact) share the same
    /// capacity- and time-bounded cache, so repeated turns for the same scope
    /// avoid rebuilding backends while total retained runtimes stay bounded.
    async fn runtime_for_scope(&self, scope: &MemoryScope) -> Result<Arc<ScopedRetrievalRuntime>> {
        if let Some(runtime) = self.scoped_runtimes.get(scope).await {
            return Ok(runtime);
        }

        let runtime = Arc::new(self.build_runtime_for_scope(scope).await?);
        self.scoped_runtimes
            .insert(scope.clone(), runtime.clone())
            .await;
        Ok(runtime)
    }

    async fn build_runtime_for_scope(&self, scope: &MemoryScope) -> Result<ScopedRetrievalRuntime> {
        self.runtime_factory
            .build_runtime(scope, &self.config, &self.pool, self.assume_app_role)
            .await
    }
}

fn memory_evidence_source_metadata(hit: &RetrievalHit) -> Result<MemoryEvidenceSourceMetadata> {
    let source_session_id = hit
        .node
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("source_session_id"))
        .filter(|value| !value.is_null());
    let source_turn_seq = hit
        .node
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("source_turn_seq"))
        .filter(|value| !value.is_null());

    match (source_session_id, source_turn_seq) {
        (None, None) => Ok(MemoryEvidenceSourceMetadata {
            graph_uid: hit.uid,
            source_session_id: None,
            source_turn_seq: None,
        }),
        (Some(session_id), Some(turn_seq)) => {
            let session_id = session_id.as_str().ok_or_else(|| {
                MoaError::ValidationError(format!(
                    "graph memory hit {} has non-string source_session_id",
                    hit.uid
                ))
            })?;
            let session_id = uuid::Uuid::parse_str(session_id).map_err(|error| {
                MoaError::ValidationError(format!(
                    "graph memory hit {} has invalid source_session_id: {error}",
                    hit.uid
                ))
            })?;
            let turn_seq = turn_seq.as_u64().ok_or_else(|| {
                MoaError::ValidationError(format!(
                    "graph memory hit {} has invalid source_turn_seq",
                    hit.uid
                ))
            })?;
            Ok(MemoryEvidenceSourceMetadata {
                graph_uid: hit.uid,
                source_session_id: Some(SessionId(session_id)),
                source_turn_seq: Some(turn_seq),
            })
        }
        _ => Err(MoaError::ValidationError(format!(
            "graph memory hit {} has incomplete source session metadata",
            hit.uid
        ))),
    }
}

#[async_trait]
impl ContextProcessor for GraphMemoryRetriever {
    fn name(&self) -> &str {
        "graph_memory"
    }

    fn stage(&self) -> u8 {
        7
    }

    fn parallelizable(&self) -> bool {
        true
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        match self.fetch(ctx).await? {
            Some(apply) => apply(ctx),
            None => Ok(ProcessorOutput::default()),
        }
    }

    async fn fetch(&self, ctx: &WorkingContext) -> Result<Option<StageApply>> {
        let policy = MemoryAdmissionPolicy::from_working_context(ctx)?;
        if !policy.is_enabled() {
            return Ok(Some(Box::new(|_ctx| {
                Ok(ProcessorOutput {
                    items_excluded: vec!["graph_memory".to_string()],
                    excluded_items: vec![ExcludedItem {
                        item: "graph_memory".to_string(),
                        reason: "disabled by pinned agent knowledge policy".to_string(),
                    }],
                    ..ProcessorOutput::default()
                })
            })));
        }
        // Resolve the retrieval query and run all retrieval I/O against the
        // immutable turn context. When query rewrite ran (the default), the
        // query comes from its metadata, written before this concurrent group;
        // otherwise it is the turn's latest user message as of this read, which
        // precedes any skill manifest a peer stage appends during apply.
        let Some(query) = extract_search_query(ctx) else {
            return Ok(Some(Box::new(|_ctx| Ok(ProcessorOutput::default()))));
        };

        // The router is the single dispatch point: it classifies the turn once
        // from lexical features (no LLM, no I/O) and every branch below keys on
        // the strategy enum rather than accreting boolean flags.
        let result_limit = policy.result_limit(self.result_limit);
        let (strategy, hits) = self
            .retrieve_admitted_hits(ctx, &query, &policy, result_limit)
            .await?;
        let strategy_label: &'static str = strategy.as_str();
        if strategy == RetrievalStrategy::Skip {
            return Ok(Some(Box::new(move |ctx: &mut WorkingContext| {
                set_offer_retrieval_tools(ctx, strategy, true);
                Ok(ProcessorOutput {
                    items_excluded: vec!["graph_memory".to_string()],
                    excluded_items: vec![ExcludedItem {
                        item: "graph_memory".to_string(),
                        reason: "router: skip (no retrieval intent)".to_string(),
                    }],
                    metadata: router_strategy_metadata(strategy_label),
                    ..ProcessorOutput::default()
                })
            })));
        }

        if hits.is_empty() {
            return Ok(Some(Box::new(move |ctx: &mut WorkingContext| {
                set_offer_retrieval_tools(ctx, strategy, true);
                Ok(ProcessorOutput {
                    metadata: router_strategy_metadata(strategy_label),
                    ..ProcessorOutput::default()
                })
            })));
        }

        let apply: StageApply = Box::new(move |ctx: &mut WorkingContext| {
            set_offer_retrieval_tools(ctx, strategy, false);
            let tokens_before = ctx.token_count;
            let memory_budget =
                (ctx.token_budget / MEMORY_BUDGET_DIVISOR).max(MIN_PAGE_EXCERPT_TOKENS);
            // F23: cap the whole injected block by the memory budget rather than a
            // per-hit floor (which let N hits reach 96·N > memory_budget). Reserve
            // the outer memory-reminder wrapper so the injected message — not just
            // the inner rendered section — stays within budget, then render the
            // largest ranked-hit prefix that fits.
            let evidence_budget = memory_budget.saturating_sub(memory_reminder_wrapper_tokens());
            let budgeted = render_memory_context_with_budget(&hits, evidence_budget);
            let omitted = hits.len().saturating_sub(budgeted.hit_count);
            if omitted > 0 {
                tracing::debug!(
                    omitted_hits = omitted,
                    rendered_hits = budgeted.hit_count,
                    evidence_budget,
                    "graph-memory injection omitted ranked hits to fit the memory budget"
                );
                metrics::counter!("moa_memory_injection_omitted_hits_total")
                    .increment(omitted as u64);
            }
            if budgeted.hit_count == 0 {
                // The budget could not fit even one hit; inject nothing rather than
                // an empty reminder wrapper.
                return Ok(ProcessorOutput {
                    items_excluded: vec!["graph_memory".to_string()],
                    excluded_items: vec![ExcludedItem {
                        item: "graph_memory".to_string(),
                        reason: "memory budget too small to render any ranked hit".to_string(),
                    }],
                    metadata: router_strategy_metadata(strategy_label),
                    ..ProcessorOutput::default()
                });
            }
            let rendered = budgeted.rendered;

            let reminder = format!(
                "{MEMORY_REMINDER_PREFIX}\n{}\n</memory-reminder>",
                rendered.section
            );
            let insertion_index = trailing_user_insertion_index(&ctx.messages);
            ctx.insert_message(
                insertion_index,
                ContextMessage::user(reminder).with_source_refs(rendered.source_refs),
            );

            Ok(ProcessorOutput {
                tokens_added: ctx.token_count.saturating_sub(tokens_before),
                items_included: rendered.items_included,
                metadata: router_strategy_metadata(strategy_label),
                ..ProcessorOutput::default()
            })
        });
        Ok(Some(apply))
    }
}

async fn embed_query(embedder: &dyn EmbeddingProvider, query: &str) -> Result<Vec<f32>> {
    let query_input = vec![query.to_string()];
    let embed_started = std::time::Instant::now();
    let mut embeddings = embedder.embed(&query_input).await?;
    metrics::histogram!("moa_retrieval_embedder_seconds")
        .record(embed_started.elapsed().as_secs_f64());
    embeddings.pop().ok_or_else(|| {
        MoaError::ProviderError("graph memory embedder returned no query embedding".to_string())
    })
}

use super::trailing_user_insertion_index;

/// Token cost of the outer `<memory-reminder>` wrapper added around the rendered
/// memory section, reserved from the memory budget so the whole injected message
/// stays within budget.
fn memory_reminder_wrapper_tokens() -> usize {
    moa_core::types::context::estimate_text_tokens(&format!(
        "{MEMORY_REMINDER_PREFIX}\n\n</memory-reminder>"
    ))
}

/// Builds the processor-output metadata that records the router's chosen
/// strategy, so evals and tests can observe the routing decision for a turn.
fn router_strategy_metadata(
    strategy_label: &'static str,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "router_strategy".to_string(),
        serde_json::Value::String(strategy_label.to_string()),
    );
    metadata
}

/// Records the agentic-memory-tool gate decision into the working context so the
/// harness can surface the tools onto this turn when appropriate.
fn set_offer_retrieval_tools(
    ctx: &mut WorkingContext,
    strategy: RetrievalStrategy,
    hits_empty: bool,
) {
    ctx.insert_metadata(
        OFFER_RETRIEVAL_TOOLS_METADATA_KEY,
        serde_json::Value::Bool(should_offer_retrieval_tools(strategy, hits_empty)),
    );
}

fn extract_search_query(ctx: &WorkingContext) -> Option<String> {
    if let Some(query) = ctx
        .metadata()
        .get("query_rewrite")
        .and_then(query_from_rewrite_metadata)
    {
        return Some(query);
    }

    extract_search_query_from_messages(&ctx.messages)
}

fn should_disable_graph_expansion(scope_plan: &RetrievalScopePlan) -> bool {
    matches!(
        scope_plan.source_tier(),
        crate::retrieval::SourceTier::TenantKnowledge
    )
}

fn query_from_rewrite_metadata(value: &serde_json::Value) -> Option<String> {
    let result = serde_json::from_value::<QueryRewriteResult>(value.clone()).ok()?;
    let query = result.retrieval_query.trim();
    (!query.is_empty()).then(|| query.to_string())
}

fn extract_search_query_from_messages(messages: &[ContextMessage]) -> Option<String> {
    let text = messages
        .iter()
        .rev()
        .find_map(|message| match message.role {
            moa_core::types::context::MessageRole::User => Some(message.content.as_str()),
            _ => None,
        })?;
    let query = text.trim();
    (!query.is_empty()).then(|| query.to_string())
}

pub(crate) fn extract_search_keywords(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "about", "after", "again", "agent", "answer", "around", "because", "before", "being",
        "between", "could", "explain", "find", "from", "have", "into", "just", "like", "make",
        "need", "please", "respond", "should", "that", "the", "their", "them", "there", "these",
        "they", "this", "what", "when", "where", "which", "with", "would", "your",
    ];

    let mut keywords = Vec::new();
    for token in text
        .split(|character: char| {
            !(character.is_alphanumeric() || character == '_' || character == '-')
        })
        .map(str::trim)
        .filter(|token| token.len() >= 3)
    {
        let normalized = token.to_ascii_lowercase();
        if STOPWORDS.contains(&normalized.as_str()) || keywords.contains(&normalized) {
            continue;
        }
        keywords.push(normalized);
        if keywords.len() >= 6 {
            break;
        }
    }

    keywords
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_core::{
        traits::ContextProcessor, types::agent::AgentContext, types::agent::AgentKnowledgePolicy,
        types::agent::AgentKnowledgeScopeMode, types::agent::AgentPolicySnapshot,
        types::channel::Channel, types::contact::ContactId, types::contact::ContactRef,
        types::contact::ContactVerificationState, types::context::ContextMessage,
        types::context::WorkingContext, types::identifiers::ModelId, types::identifiers::SessionId,
        types::identifiers::TenantId, types::model::ModelCapabilities, types::model::TokenPricing,
        types::model::ToolCallFormat, types::query_rewrite::QueryRewriteResult,
        types::session::SessionMeta,
    };
    use moa_lineage_core::TurnId;
    use moa_memory_graph::{
        EdgeLabel, EdgeWriteIntent, GraphExpansionHit, GraphStore, NodeIndexRow, NodeLabel,
        NodeWriteIntent, PiiClass,
    };
    use moa_memory_types::MemoryScope;
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use crate::planning::Strategy;
    use crate::retrieval::{MemoryAdmissionPolicy, RetrievalScopePlan};

    use super::{
        GraphMemoryRetriever, MemoryEvidenceRequest, ScopedRetrievalRuntime,
        ScopedRetrievalRuntimeFactory, SharedGraphMemoryRetriever, extract_search_keywords,
        extract_search_query, should_disable_graph_expansion,
    };

    fn retrieval_scopes_from_context(ctx: &WorkingContext) -> Vec<RetrievalScopePlan> {
        MemoryAdmissionPolicy::from_working_context(ctx)
            .expect("memory admission policy should parse")
            .plans()
            .to_vec()
    }

    #[derive(Debug)]
    struct NoopGraphStore;

    #[async_trait]
    impl GraphStore for NoopGraphStore {
        async fn create_node(&self, _intent: NodeWriteIntent) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn supersede_node(
            &self,
            _old_uid: Uuid,
            _intent: NodeWriteIntent,
        ) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn invalidate_node(&self, _uid: Uuid, _reason: &str) -> moa_memory_graph::Result<()> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn hard_purge(
            &self,
            _uid: Uuid,
            _redaction_marker: &str,
        ) -> moa_memory_graph::Result<()> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn create_edge(&self, _intent: EdgeWriteIntent) -> moa_memory_graph::Result<Uuid> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn get_node(&self, _uid: Uuid) -> moa_memory_graph::Result<Option<NodeIndexRow>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn neighbors(
            &self,
            _seed: Uuid,
            _hops: u8,
            _edge_filter: Option<&[EdgeLabel]>,
            _as_of: Option<DateTime<Utc>>,
        ) -> moa_memory_graph::Result<Vec<NodeIndexRow>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn expand_seeds(
            &self,
            _seeds: &[Uuid],
            _max_hops: u8,
            _as_of: Option<DateTime<Utc>>,
            _scoring: &moa_memory_graph::GraphWalkScoring,
        ) -> moa_memory_graph::Result<Vec<GraphExpansionHit>> {
            panic!("NoopGraphStore should not be called by runtime factory tests")
        }

        async fn lookup_seeds(
            &self,
            _name: &str,
            _limit: i64,
            _as_of: Option<DateTime<Utc>>,
        ) -> moa_memory_graph::Result<Vec<NodeIndexRow>> {
            Ok(Vec::new())
        }
    }

    #[derive(Debug)]
    struct NoopPlannedRetriever;

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for NoopPlannedRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            _req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone, Debug)]
    struct CountingRuntimeFactory {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for CountingRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::config::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::error::Result<ScopedRetrievalRuntime> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                Arc::new(NoopPlannedRetriever),
            ))
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    struct RecordedRetrievalRequest {
        scope: MemoryScope,
        label_filter: Option<Vec<NodeLabel>>,
        strategy: Option<Strategy>,
    }

    #[derive(Debug)]
    struct ScriptedPlannedRetriever {
        calls: Arc<Mutex<Vec<RecordedRetrievalRequest>>>,
        hits_by_scope: HashMap<MemoryScope, Vec<crate::retrieval::RetrievalHit>>,
    }

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for ScriptedPlannedRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            self.calls
                .lock()
                .expect("scripted retriever calls lock")
                .push(RecordedRetrievalRequest {
                    scope: req.scope.clone(),
                    label_filter: req.label_filter.clone(),
                    strategy: req.strategy,
                });
            Ok(self
                .hits_by_scope
                .get(&req.scope)
                .cloned()
                .unwrap_or_default())
        }
    }

    #[derive(Clone, Debug)]
    struct ScriptedRuntimeFactory {
        retriever: Arc<ScriptedPlannedRetriever>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for ScriptedRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::config::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::error::Result<ScopedRetrievalRuntime> {
            let retriever: Arc<dyn crate::retrieval::PlannedRetriever> = self.retriever.clone();
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                retriever,
            ))
        }
    }

    /// Scripted retriever that returns hits keyed by a substring of the query
    /// text, so a deep decomposition can be proven to run each sub-query
    /// independently. Records every retrieved query text.
    #[derive(Debug)]
    struct QueryScriptedPlannedRetriever {
        queries: Arc<Mutex<Vec<String>>>,
        hits_by_marker: Vec<(String, Vec<crate::retrieval::RetrievalHit>)>,
    }

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for QueryScriptedPlannedRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            let query = req.query_text.to_ascii_lowercase();
            self.queries
                .lock()
                .expect("scripted retriever queries lock")
                .push(req.query_text.clone());
            let hits = self
                .hits_by_marker
                .iter()
                .find(|(marker, _)| query.contains(marker.as_str()))
                .map(|(_, hits)| hits.clone())
                .unwrap_or_default();
            Ok(hits)
        }
    }

    #[derive(Clone, Debug)]
    struct QueryScriptedRuntimeFactory {
        retriever: Arc<QueryScriptedPlannedRetriever>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for QueryScriptedRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::config::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::error::Result<ScopedRetrievalRuntime> {
            let retriever: Arc<dyn crate::retrieval::PlannedRetriever> = self.retriever.clone();
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                retriever,
            ))
        }
    }

    /// Retriever whose cache probe always hits, so no backend or embedding runs.
    #[derive(Debug)]
    struct CacheHitRetriever {
        retrieve_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl crate::retrieval::PlannedRetriever for CacheHitRetriever {
        async fn retrieve(
            &self,
            _planned: &crate::planning::PlannedQuery,
            _req: crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Vec<crate::retrieval::RetrievalHit>> {
            self.retrieve_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn retrieve_cached(
            &self,
            _planned: &crate::planning::PlannedQuery,
            _req: &crate::retrieval::RetrievalRequest,
        ) -> crate::retrieval::Result<Option<Vec<crate::retrieval::RetrievalHit>>> {
            Ok(Some(Vec::new()))
        }
    }

    #[derive(Clone, Debug)]
    struct CacheHitRuntimeFactory {
        retriever: Arc<CacheHitRetriever>,
    }

    #[async_trait]
    impl ScopedRetrievalRuntimeFactory for CacheHitRuntimeFactory {
        async fn build_runtime(
            &self,
            _scope: &MemoryScope,
            _config: &moa_core::config::MoaConfig,
            _pool: &sqlx::PgPool,
            _assume_app_role: bool,
        ) -> moa_core::error::Result<ScopedRetrievalRuntime> {
            let retriever: Arc<dyn crate::retrieval::PlannedRetriever> = self.retriever.clone();
            Ok(ScopedRetrievalRuntime::new(
                Arc::new(NoopGraphStore),
                retriever,
            ))
        }
    }

    #[derive(Debug)]
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl moa_core::traits::EmbeddingProvider for CountingEmbedder {
        fn model_id(&self) -> &str {
            "counting-embedder"
        }

        fn dimensions(&self) -> usize {
            4
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(inputs.iter().map(|_| vec![0.0; 4]).collect())
        }
    }

    #[tokio::test]
    async fn retrieval_skips_embedding_when_cache_probe_hits() {
        // Pins: cache-before-embed ordering — when every scope's read-time cache
        // probe hits, the stage must not call the embedding provider or the
        // retrieval backend.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let embed_calls = Arc::new(AtomicUsize::new(0));
        let retrieve_calls = Arc::new(AtomicUsize::new(0));
        let retriever = GraphMemoryRetriever::new(
            pool,
            Some(Arc::new(CountingEmbedder {
                calls: embed_calls.clone(),
            })),
        )
        .with_scoped_runtime_factory(Arc::new(CacheHitRuntimeFactory {
            retriever: Arc::new(CacheHitRetriever {
                retrieve_calls: retrieve_calls.clone(),
            }),
        }));
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "what is the cached answer",
        ));

        retriever
            .process(&mut ctx)
            .await
            .expect("cache-hit retrieval should assemble context");

        assert_eq!(
            embed_calls.load(Ordering::SeqCst),
            0,
            "a cache-probe hit must not embed the query"
        );
        assert_eq!(
            retrieve_calls.load(Ordering::SeqCst),
            0,
            "a cache-probe hit must not touch the retrieval backend"
        );
    }

    #[tokio::test]
    async fn shared_graph_memory_retriever_preserves_processor_identity() {
        // Pins: shared graph-memory runtime remains the stage-7 memory processor.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let shared = SharedGraphMemoryRetriever::new(std::sync::Arc::new(
            GraphMemoryRetriever::new(pool, None),
        ));

        assert_eq!(shared.name(), "graph_memory");
        assert_eq!(shared.stage(), 7);
    }

    #[tokio::test]
    async fn scoped_runtimes_are_reused_within_a_bounded_cache() {
        // Pins: every scope tier (tenant and contact) reuses one runtime from a
        // capacity-bounded process-wide cache, so repeated turns for the same
        // scope never rebuild backends while total retained runtimes stay
        // bounded rather than growing one-per-contact without limit.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever = GraphMemoryRetriever::new(pool, None).with_scoped_runtime_factory(
            Arc::new(CountingRuntimeFactory {
                calls: calls.clone(),
            }),
        );
        let tenant_id = TenantId::new();
        let contact_scope = MemoryScope::Contact {
            tenant_id,
            contact_id: ContactId::new(),
        };
        let tenant_scope = MemoryScope::Tenant { tenant_id };

        retriever
            .runtime_for_scope(&contact_scope)
            .await
            .expect("contact runtime should build");
        retriever
            .runtime_for_scope(&contact_scope)
            .await
            .expect("contact runtime should be reused");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "contact scope should reuse its cached runtime across turns"
        );

        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should build");
        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should be reused");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "tenant scope should reuse its cached runtime across turns"
        );

        retriever.scoped_runtimes.run_pending_tasks().await;
        assert_eq!(
            retriever.scoped_runtimes.entry_count(),
            2,
            "each distinct scope retains exactly one bounded cache entry"
        );
    }

    #[tokio::test]
    async fn scoped_runtime_factory_can_be_injected_without_postgres_stores() {
        // Pins: graph-memory tests can inject scope runtimes instead of constructing PG stores.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let calls = Arc::new(AtomicUsize::new(0));
        let retriever = GraphMemoryRetriever::new(pool, None).with_scoped_runtime_factory(
            Arc::new(CountingRuntimeFactory {
                calls: calls.clone(),
            }),
        );
        let tenant_id = TenantId::new();
        let tenant_scope = MemoryScope::Tenant { tenant_id };
        let contact_scope = MemoryScope::Contact {
            tenant_id,
            contact_id: ContactId::new(),
        };

        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should build from injected factory");
        retriever
            .runtime_for_scope(&tenant_scope)
            .await
            .expect("tenant runtime should be cached");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        retriever
            .runtime_for_scope(&contact_scope)
            .await
            .expect("contact runtime should build from injected factory");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn keyword_extraction_filters_stopwords_and_duplicates() {
        let keywords =
            extract_search_keywords("Please explain the OAuth refresh token race condition bug");

        assert_eq!(
            keywords,
            vec!["oauth", "refresh", "token", "race", "condition", "bug"]
        );
    }

    #[test]
    fn keyword_extraction_preserves_memory_article_ids() {
        let keywords = extract_search_keywords("What is news_article_001 about?");

        assert_eq!(keywords, vec!["news_article_001"]);
    }

    #[test]
    fn keyword_extraction_handles_empty_unicode_and_over_cap_input() {
        // Pins: empty input yields nothing; unicode word characters are retained
        // (only case-folded); and extraction caps at six distinct keywords.
        assert!(extract_search_keywords("").is_empty());

        assert_eq!(
            extract_search_keywords("café déjà OAuth"),
            vec!["café", "déjà", "oauth"]
        );

        let keywords = extract_search_keywords("alpha bravo charlie delta echo foxtrot golf hotel");
        assert_eq!(keywords.len(), 6);
        assert_eq!(
            keywords,
            vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]
        );
    }

    #[test]
    fn original_rewrite_metadata_uses_full_query_for_retrieval() {
        // Pins: fail-open rewrites preserve the full semantic query instead of keyword-only fallback.
        let mut ctx = WorkingContext::new(
            &SessionMeta {
                id: SessionId::new(),
                tenant_id: TenantId::new(),
                channel: Channel::Chat,
                model: ModelId::new("mock"),
                ..SessionMeta::default()
            },
            capabilities(),
        );
        ctx.insert_metadata(
            "query_rewrite",
            serde_json::to_value(QueryRewriteResult::original(
                "Please explain the OAuth refresh token race condition bug",
            ))
            .expect("rewrite result should serialize"),
        );

        assert_eq!(
            extract_search_query(&ctx),
            Some("Please explain the OAuth refresh token race condition bug".to_string())
        );
    }

    #[test]
    fn original_rewrite_metadata_uses_latest_user_query_for_retrieval() {
        // Pins: skipped rewrite metadata preserves the full natural-language retrieval query.
        let mut ctx = WorkingContext::new(
            &SessionMeta {
                id: SessionId::new(),
                tenant_id: TenantId::new(),
                channel: Channel::Chat,
                model: ModelId::new("mock"),
                ..SessionMeta::default()
            },
            capabilities(),
        );
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "Please explain the OAuth refresh token race condition bug",
        ));

        assert_eq!(
            extract_search_query(&ctx),
            Some("Please explain the OAuth refresh token race condition bug".to_string())
        );
    }

    #[test]
    fn lineage_context_uses_compiled_turn_id_metadata() {
        // Pins: retrieval sidecar rows can join directly to turn-scoped lineage rows.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        let turn_id = TurnId::new_v7();
        ctx.insert_metadata("_moa.turn_id", serde_json::json!(turn_id.0.to_string()));
        ctx.insert_metadata("_moa.turn_seq", serde_json::json!(42));

        let lineage = super::lineage_context_from_context(&ctx);

        assert_eq!(lineage.session_id, session.id);
        assert_eq!(lineage.turn_id, Some(turn_id));
        assert_eq!(lineage.turn_seq, 42);
    }

    #[tokio::test]
    async fn tenant_contact_knowledge_retrieval_disabled_policy_returns_no_memory() {
        // Pins: disabled configured-agent knowledge policy bypasses graph memory retrieval.
        let contact_id = ContactId::new();
        let mut session =
            contact_session(contact_id, ContactVerificationState::Verified, Vec::new());
        session.agent_context = Some(agent_context_with_knowledge_policy(AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        }));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), HashMap::new());
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "Find relevant knowledge",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("disabled memory retrieval should not fail");

        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").len(),
            0,
            "disabled policy should not call scoped retrieval"
        );
        assert_eq!(ctx.messages.len(), 1, "disabled policy inserted memory");
        assert_eq!(output.items_included, Vec::<String>::new());
        assert_eq!(output.items_excluded, vec!["graph_memory".to_string()]);
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_contact_context_plans_tenant_and_contact_scopes() {
        // Pins: contact sessions retrieve tenant knowledge plus only the current contact memory.
        let contact_id = ContactId::new();
        let session = contact_session(contact_id, ContactVerificationState::Verified, Vec::new());
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx);

        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0].scope(),
            &MemoryScope::Tenant {
                tenant_id: session.tenant_id,
            }
        );
        assert_eq!(
            plan[0].source_tier(),
            crate::retrieval::SourceTier::TenantKnowledge
        );
        assert_eq!(
            plan[0].label_filter(),
            Some(
                [
                    NodeLabel::Document,
                    NodeLabel::Chunk,
                    NodeLabel::ContactGroup,
                ]
                .as_slice()
            )
        );
        assert_eq!(
            plan[1].scope(),
            &MemoryScope::Contact {
                tenant_id: session.tenant_id,
                contact_id,
            }
        );
        assert_eq!(
            plan[1].source_tier(),
            crate::retrieval::SourceTier::UserMemory
        );
        assert_eq!(plan[1].label_filter(), None);
    }

    #[test]
    fn tenant_knowledge_retrieval_disables_graph_expansion() {
        // Pins: tenant KB retrieval stays on direct vector/lexical hits; graph
        // expansion remains available for contact memory where fact-neighbor
        // traversal is part of the retrieval model.
        let contact_id = ContactId::new();
        let session = contact_session(contact_id, ContactVerificationState::Verified, Vec::new());
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx);

        let tenant_plan = plan
            .iter()
            .find(|scope_plan| {
                scope_plan.source_tier() == crate::retrieval::SourceTier::TenantKnowledge
            })
            .expect("tenant knowledge plan should exist");
        let contact_plan = plan
            .iter()
            .find(|scope_plan| scope_plan.source_tier() == crate::retrieval::SourceTier::UserMemory)
            .expect("contact memory plan should exist");

        assert!(should_disable_graph_expansion(tenant_plan));
        assert!(!should_disable_graph_expansion(contact_plan));
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_no_contact_plans_tenant_only() {
        // Pins: sessions without an admitted contact retrieve tenant knowledge only.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: None,
            ..SessionMeta::default()
        };
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx);

        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].scope(),
            &MemoryScope::Tenant {
                tenant_id: session.tenant_id,
            }
        );
        assert_eq!(
            plan[0].source_tier(),
            crate::retrieval::SourceTier::TenantKnowledge
        );
    }

    #[test]
    fn tenant_contact_knowledge_retrieval_ignores_linked_cross_contact_scopes() {
        // Pins: verified contacts do not inherit linked-contact memory by default.
        let contact_id = ContactId::new();
        let linked_contact_id = ContactId::new();
        let session = contact_session(
            contact_id,
            ContactVerificationState::Verified,
            vec![contact_id, linked_contact_id, linked_contact_id],
        );
        let ctx = WorkingContext::new(&session, capabilities());
        let plan = retrieval_scopes_from_context(&ctx);

        assert_eq!(plan.len(), 2);
        assert!(plan.iter().any(|scope_plan| {
            *scope_plan.scope()
                == MemoryScope::Contact {
                    tenant_id: session.tenant_id,
                    contact_id,
                }
        }));
        assert!(!plan.iter().any(|scope_plan| {
            *scope_plan.scope()
                == MemoryScope::Contact {
                    tenant_id: session.tenant_id,
                    contact_id: linked_contact_id,
                }
        }));
    }

    #[tokio::test]
    async fn tenant_contact_knowledge_retrieval_context_separates_tenant_and_user_sections() {
        // Pins: prompt context keeps tenant knowledge and current-contact memory separate.
        let contact_id = ContactId::new();
        let linked_contact_id = ContactId::new();
        let session = contact_session(
            contact_id,
            ContactVerificationState::Verified,
            vec![linked_contact_id],
        );
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let contact_scope = MemoryScope::Contact {
            tenant_id: session.tenant_id,
            contact_id,
        };
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(
            tenant_scope.clone(),
            vec![
                retrieval_hit(
                    Uuid::from_u128(0x10),
                    session.tenant_id,
                    None,
                    NodeLabel::Fact,
                    "tenant",
                    crate::retrieval::SourceTier::TenantKnowledge,
                    "tenant operational fact",
                    "tenant fact should not be tenant knowledge",
                    0.99,
                ),
                retrieval_hit(
                    Uuid::from_u128(0x11),
                    session.tenant_id,
                    None,
                    NodeLabel::Chunk,
                    "tenant",
                    crate::retrieval::SourceTier::TenantKnowledge,
                    "tenant runbook chunk",
                    "tenant knowledge answer",
                    0.90,
                ),
            ],
        );
        hits_by_scope.insert(
            contact_scope,
            vec![
                retrieval_hit(
                    Uuid::from_u128(0x20),
                    session.tenant_id,
                    Some(contact_id),
                    NodeLabel::Fact,
                    "contact",
                    crate::retrieval::SourceTier::UserMemory,
                    "current contact preference",
                    "current user memory answer",
                    0.80,
                ),
                retrieval_hit(
                    Uuid::from_u128(0x21),
                    session.tenant_id,
                    Some(linked_contact_id),
                    NodeLabel::Fact,
                    "contact",
                    crate::retrieval::SourceTier::UserMemory,
                    "other contact preference",
                    "cross contact memory should not leak",
                    0.95,
                ),
            ],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), hits_by_scope);
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "Tell me the tenant and user memory answer",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("scripted retrieval should assemble context");

        let calls = calls.lock().expect("scripted retriever calls lock").clone();
        assert_eq!(
            calls,
            vec![
                RecordedRetrievalRequest {
                    scope: tenant_scope,
                    label_filter: Some(vec![
                        NodeLabel::Document,
                        NodeLabel::Chunk,
                        NodeLabel::ContactGroup,
                    ]),
                    strategy: Some(Strategy::VectorFirst),
                },
                RecordedRetrievalRequest {
                    scope: MemoryScope::Contact {
                        tenant_id: session.tenant_id,
                        contact_id,
                    },
                    label_filter: None,
                    strategy: Some(Strategy::Both),
                },
            ],
            "retriever should call tenant knowledge and current-contact scopes only"
        );
        assert_eq!(
            output.items_included,
            vec![
                "graph:Chunk:00000000-0000-0000-0000-000000000011".to_string(),
                "graph:Fact:00000000-0000-0000-0000-000000000020".to_string(),
            ]
        );
        let memory_message = ctx
            .messages
            .first()
            .expect("memory reminder should be inserted before user message");
        assert!(memory_message.content.contains("<knowledge_context>"));
        assert!(memory_message.content.contains("<tenant_knowledge>"));
        assert!(memory_message.content.contains("<user_memory>"));
        let tenant_section = section_between(
            &memory_message.content,
            "<tenant_knowledge>",
            "</tenant_knowledge>",
        );
        let user_section =
            section_between(&memory_message.content, "<user_memory>", "</user_memory>");
        assert!(tenant_section.contains("tenant knowledge answer"));
        assert!(!tenant_section.contains("current user memory answer"));
        assert!(user_section.contains("current user memory answer"));
        assert!(!user_section.contains("tenant knowledge answer"));
        assert!(
            !memory_message
                .content
                .contains("tenant fact should not be tenant knowledge")
        );
        assert!(
            !memory_message
                .content
                .contains("cross contact memory should not leak")
        );
    }

    #[tokio::test]
    async fn router_skip_turn_excludes_graph_memory_without_retrieval() {
        // Pins: an acknowledgement turn routes to Skip, so the stage never calls
        // the retrieval backend, inserts no memory reminder, and reports
        // graph_memory as excluded with the router reason.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), HashMap::new());
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: None,
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user("thanks!"));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("skip routing should not fail");

        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").len(),
            0,
            "a skipped turn must not touch the retrieval backend"
        );
        assert_eq!(
            ctx.messages.len(),
            1,
            "skip must not insert a memory reminder"
        );
        assert_eq!(output.items_excluded, vec!["graph_memory".to_string()]);
        assert_eq!(
            output.excluded_items,
            vec![moa_core::types::context::ExcludedItem {
                item: "graph_memory".to_string(),
                reason: "router: skip (no retrieval intent)".to_string(),
            }]
        );
        assert_eq!(
            output.metadata.get("router_strategy"),
            Some(&serde_json::Value::String("skip".to_string()))
        );
        assert_eq!(
            ctx.metadata()
                .get(super::OFFER_RETRIEVAL_TOOLS_METADATA_KEY),
            Some(&serde_json::Value::Bool(false)),
            "a skip turn must not offer the agentic memory tools"
        );
    }

    /// Builds a tenant-only session used by the agentic-tool gate tests.
    fn tenant_only_session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: None,
            ..SessionMeta::default()
        }
    }

    #[tokio::test]
    async fn fast_turn_with_hits_does_not_offer_agentic_memory_tools() {
        // Pins: a Fast-routed turn that injected memory hits does NOT gate the
        // agentic memory tools on — the common cheap path keeps a stable loadout.
        let session = tenant_only_session();
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(
            tenant_scope,
            vec![retrieval_hit(
                Uuid::from_u128(0x50),
                session.tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                crate::retrieval::SourceTier::TenantKnowledge,
                "rotation policy",
                "API keys rotate every 90 days",
                0.90,
            )],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls, hits_by_scope);
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "What is my API key rotation policy?",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("fast retrieval should assemble context");

        assert_eq!(
            output.metadata.get("router_strategy"),
            Some(&serde_json::Value::String("fast".to_string()))
        );
        assert_eq!(
            ctx.metadata()
                .get(super::OFFER_RETRIEVAL_TOOLS_METADATA_KEY),
            Some(&serde_json::Value::Bool(false)),
            "a fast turn with hits must not offer the agentic memory tools"
        );
    }

    #[tokio::test]
    async fn fast_turn_without_hits_offers_agentic_memory_tools() {
        // Pins: when a retrieval-intent turn injects nothing, the CRAG-style
        // fallback gates the agentic memory tools on so the model can dig.
        let session = tenant_only_session();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls, HashMap::new());
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "What is my API key rotation policy?",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("fast retrieval should not fail on empty hits");

        assert_eq!(
            output.metadata.get("router_strategy"),
            Some(&serde_json::Value::String("fast".to_string()))
        );
        assert_eq!(
            ctx.metadata()
                .get(super::OFFER_RETRIEVAL_TOOLS_METADATA_KEY),
            Some(&serde_json::Value::Bool(true)),
            "an empty fast turn must offer the agentic memory tools"
        );
    }

    #[tokio::test]
    async fn agentic_turn_offers_agentic_memory_tools_even_with_hits() {
        // Pins: an agentic-routed turn keeps Fast injection (hits still render)
        // AND gates the agentic memory tools on so the model can iterate.
        let session = tenant_only_session();
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(
            tenant_scope,
            vec![retrieval_hit(
                Uuid::from_u128(0x51),
                session.tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                crate::retrieval::SourceTier::TenantKnowledge,
                "prior incident",
                "the auth outage was caused by a rotated key",
                0.90,
            )],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls, hits_by_scope);
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "Investigate what we know about the auth outage",
        ));

        let output = retriever
            .process(&mut ctx)
            .await
            .expect("agentic retrieval should assemble context");

        assert_eq!(
            output.metadata.get("router_strategy"),
            Some(&serde_json::Value::String("agentic".to_string()))
        );
        assert_eq!(
            ctx.metadata()
                .get(super::OFFER_RETRIEVAL_TOOLS_METADATA_KEY),
            Some(&serde_json::Value::Bool(true)),
            "an agentic turn must offer the agentic memory tools"
        );
        assert!(
            ctx.messages
                .first()
                .expect("memory reminder inserted")
                .content
                .contains("the auth outage was caused by a rotated key"),
            "agentic turns keep Fast injection of the top-k hits"
        );
    }

    #[tokio::test]
    async fn router_deep_query_runs_each_subquery_and_fuses_hits() {
        // Pins: a deep-shaped multi-hop query decomposes into (capped) sub-queries,
        // issues more than one backend retrieval, and the injected memory contains
        // the fused hits from both sub-queries.
        let tenant_id = TenantId::new();
        let inner_hit = retrieval_hit(
            Uuid::from_u128(0x30),
            tenant_id,
            None,
            NodeLabel::Chunk,
            "tenant",
            crate::retrieval::SourceTier::TenantKnowledge,
            "dependency chunk",
            "svc depends on the shared crypto library",
            0.90,
        );
        let outer_hit = retrieval_hit(
            Uuid::from_u128(0x31),
            tenant_id,
            None,
            NodeLabel::Chunk,
            "tenant",
            crate::retrieval::SourceTier::TenantKnowledge,
            "ownership chunk",
            "the platform team owns the shared crypto library",
            0.80,
        );
        let queries = Arc::new(Mutex::new(Vec::new()));
        let retriever = Arc::new(QueryScriptedPlannedRetriever {
            queries: queries.clone(),
            hits_by_marker: vec![
                ("depends".to_string(), vec![inner_hit]),
                ("owns".to_string(), vec![outer_hit]),
            ],
        });
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let graph_memory = GraphMemoryRetriever::new(pool, None)
            .with_scoped_runtime_factory(Arc::new(QueryScriptedRuntimeFactory { retriever }));

        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: None,
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(moa_core::types::context::ContextMessage::user(
            "Which team owns the library that svc depends on?",
        ));

        let output = graph_memory
            .process(&mut ctx)
            .await
            .expect("deep retrieval should assemble context");

        let recorded = queries
            .lock()
            .expect("scripted retriever queries lock")
            .clone();
        assert_eq!(
            recorded.len(),
            2,
            "deep decomposition runs one retrieval per sub-query (capped at 2): {recorded:?}"
        );
        assert!(
            recorded.iter().any(|query| query.contains("depends on")),
            "one sub-query retrieves the inner dependency fact: {recorded:?}"
        );
        assert!(
            recorded.iter().any(|query| query.contains("owns")),
            "one sub-query retrieves the outer ownership fact: {recorded:?}"
        );
        assert_eq!(
            output.metadata.get("router_strategy"),
            Some(&serde_json::Value::String("deep".to_string()))
        );
        let memory_message = ctx
            .messages
            .first()
            .expect("memory reminder should be inserted before user message");
        assert!(
            memory_message
                .content
                .contains("svc depends on the shared crypto library"),
            "fused memory must include the inner sub-query hit"
        );
        assert!(
            memory_message
                .content
                .contains("the platform team owns the shared crypto library"),
            "fused memory must include the outer sub-query hit"
        );
    }

    pub(super) async fn verify_evidence_rejects_zero_token_budget_before_retrieval() {
        // Pins: callers must provide a positive explicit evidence budget; an
        // invalid request fails before any graph/runtime access.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), HashMap::new());
        let session = tenant_only_session();
        let ctx = WorkingContext::new(&session, capabilities());

        let error = retriever
            .retrieve_evidence(&ctx, MemoryEvidenceRequest::new("rotation policy", 0))
            .await
            .expect_err("zero evidence budget must fail");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::ValidationError(ref message)
                    if message == "memory evidence token budget must be positive"
            ),
            "zero budget should return the dedicated validation error: {error}"
        );
        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").len(),
            0,
            "budget validation must happen before retrieval"
        );
    }

    pub(super) async fn verify_evidence_returns_only_admitted_hits_with_aligned_typed_sources() {
        // Pins: the public seam uses the same tenant-knowledge/current-contact
        // admission policy as stage 7 and reverses slow-path source provenance
        // without scraping the rendered prompt.
        let contact_id = ContactId::new();
        let other_contact_id = ContactId::new();
        let session = contact_session(
            contact_id,
            ContactVerificationState::Verified,
            vec![other_contact_id],
        );
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let contact_scope = MemoryScope::Contact {
            tenant_id: session.tenant_id,
            contact_id,
        };
        let source_session_id = SessionId::new();

        let mut tenant_hits = vec![
            retrieval_hit(
                Uuid::from_u128(0x81),
                session.tenant_id,
                None,
                NodeLabel::Fact,
                "tenant",
                crate::retrieval::SourceTier::TenantKnowledge,
                "tenant fact",
                "tenant fact must not cross the knowledge label admission boundary",
                0.99,
            ),
            retrieval_hit(
                Uuid::from_u128(0x82),
                session.tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                crate::retrieval::SourceTier::TenantKnowledge,
                "runbook",
                "Tenant runbook evidence.",
                0.90,
            ),
        ];
        let mut contact_hit = retrieval_hit(
            Uuid::from_u128(0x83),
            session.tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact",
            crate::retrieval::SourceTier::UserMemory,
            "preference",
            "Current contact prefers concise summaries.",
            0.80,
        );
        contact_hit.node.properties_summary = Some(json!({
            "summary": "Current contact prefers concise summaries.",
            "source_session_id": source_session_id.to_string(),
            "source_turn_seq": 7,
        }));
        let other_contact_hit = retrieval_hit(
            Uuid::from_u128(0x84),
            session.tenant_id,
            Some(other_contact_id),
            NodeLabel::Fact,
            "contact",
            crate::retrieval::SourceTier::UserMemory,
            "other preference",
            "Other contact memory must not leak.",
            0.98,
        );
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(tenant_scope.clone(), std::mem::take(&mut tenant_hits));
        hits_by_scope.insert(contact_scope.clone(), vec![contact_hit, other_contact_hit]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), hits_by_scope);
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.append_message(ContextMessage::user(
            "What does memory say about summaries?",
        ));
        let original_messages = ctx.messages.clone();

        let response = retriever
            .retrieve_evidence(
                &ctx,
                MemoryEvidenceRequest::new("What does memory say about summaries?", 512),
            )
            .await
            .expect("admitted evidence should render");

        assert_eq!(
            ctx.messages, original_messages,
            "the evidence seam is read-only"
        );
        assert_eq!(
            response.hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![Uuid::from_u128(0x82), Uuid::from_u128(0x83)]
        );
        assert_eq!(response.source_refs.len(), 2);
        assert_eq!(response.source_metadata.len(), 2);
        assert_eq!(
            response
                .source_refs
                .iter()
                .map(|source| source.source_uid)
                .collect::<Vec<_>>(),
            vec![Some(Uuid::from_u128(0x82)), Some(Uuid::from_u128(0x83))]
        );
        assert_eq!(
            response.source_metadata,
            vec![
                super::MemoryEvidenceSourceMetadata {
                    graph_uid: Uuid::from_u128(0x82),
                    source_session_id: None,
                    source_turn_seq: None,
                },
                super::MemoryEvidenceSourceMetadata {
                    graph_uid: Uuid::from_u128(0x83),
                    source_session_id: Some(source_session_id),
                    source_turn_seq: Some(7),
                },
            ]
        );
        assert!(
            response
                .rendered_evidence
                .contains("Tenant runbook evidence.")
        );
        assert!(
            response
                .rendered_evidence
                .contains("Current contact prefers concise summaries.")
        );
        assert!(!response.rendered_evidence.contains("tenant fact must not"));
        assert!(!response.rendered_evidence.contains("Other contact memory"));
        for source in &response.source_refs {
            let excerpt = source.excerpt.as_deref().expect("evidence excerpt");
            assert!(
                response.rendered_evidence.contains(excerpt),
                "every source excerpt must exactly occur in rendered evidence"
            );
        }
        assert_eq!(
            response.consumed_evidence_tokens,
            moa_core::types::context::estimate_text_tokens(&response.rendered_evidence)
        );
        assert!(response.consumed_evidence_tokens <= 512);
        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").clone(),
            vec![
                RecordedRetrievalRequest {
                    scope: tenant_scope,
                    label_filter: Some(vec![
                        NodeLabel::Document,
                        NodeLabel::Chunk,
                        NodeLabel::ContactGroup,
                    ]),
                    strategy: Some(Strategy::VectorFirst),
                },
                RecordedRetrievalRequest {
                    scope: contact_scope,
                    label_filter: None,
                    strategy: Some(Strategy::Both),
                },
            ]
        );
    }

    pub(super) async fn verify_evidence_token_budget_caps_exact_rendered_output() {
        // Pins: a tight evidence budget truncates the production-rendered
        // excerpt and reports the same token estimate used by context assembly.
        let session = tenant_only_session();
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let long_summary = "rotation evidence ".repeat(300);
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(
            tenant_scope,
            vec![retrieval_hit(
                Uuid::from_u128(0x85),
                session.tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                crate::retrieval::SourceTier::TenantKnowledge,
                "rotation",
                &long_summary,
                0.90,
            )],
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls, hits_by_scope);
        let ctx = WorkingContext::new(&session, capabilities());
        let token_budget = 256;

        let response = retriever
            .retrieve_evidence(
                &ctx,
                MemoryEvidenceRequest::new("What is the rotation evidence?", token_budget),
            )
            .await
            .expect("tight evidence budget should render a truncated hit");

        assert_eq!(response.hits.len(), 1, "top admitted hit should fit");
        assert_eq!(response.source_refs.len(), 1);
        assert_eq!(
            response.consumed_evidence_tokens,
            moa_core::types::context::estimate_text_tokens(&response.rendered_evidence)
        );
        assert!(
            response.consumed_evidence_tokens <= token_budget,
            "rendered evidence used {} tokens over budget {token_budget}",
            response.consumed_evidence_tokens
        );
        let excerpt = response.source_refs[0]
            .excerpt
            .as_deref()
            .expect("truncated evidence excerpt");
        assert!(excerpt.ends_with("..."));
        assert!(excerpt.len() < long_summary.len());
        assert!(response.rendered_evidence.contains(excerpt));
    }

    pub(super) async fn verify_evidence_ranked_depth_is_request_local_and_rendering_is_separate() {
        // Pins: evaluation can request more than the stage-7 default without
        // mutating the retriever, and a tight reader budget does not erase the
        // admitted ranking that retrieval metrics consume.
        let mut session = tenant_only_session();
        session.agent_context = Some(agent_context_with_knowledge_policy(AgentKnowledgePolicy {
            retrieval_budget: Some(8),
            ..AgentKnowledgePolicy::default()
        }));
        let tenant_scope = MemoryScope::Tenant {
            tenant_id: session.tenant_id,
        };
        let hits = (0_u128..6)
            .map(|index| {
                retrieval_hit(
                    Uuid::from_u128(0x900 + index),
                    session.tenant_id,
                    None,
                    NodeLabel::Chunk,
                    "tenant",
                    crate::retrieval::SourceTier::TenantKnowledge,
                    &format!("ranked-{index}"),
                    &format!("{} {index}", "ranked evidence".repeat(120)),
                    1.0 - (index as f64 * 0.01),
                )
            })
            .collect();
        let mut hits_by_scope = HashMap::new();
        hits_by_scope.insert(tenant_scope, hits);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls, hits_by_scope);
        let ctx = WorkingContext::new(&session, capabilities());

        let default_response = retriever
            .retrieve_evidence(
                &ctx,
                MemoryEvidenceRequest::new("What is the ranked evidence?", 256),
            )
            .await
            .expect("default evidence retrieval");
        let expanded_response = retriever
            .retrieve_evidence(
                &ctx,
                MemoryEvidenceRequest::new("What is the ranked evidence?", 256)
                    .with_ranked_occurrence_depth(6)
                    .expect("valid ranked occurrence depth"),
            )
            .await
            .expect("expanded evidence retrieval");

        assert_eq!(default_response.hits.len(), 4);
        assert_eq!(expanded_response.hits.len(), 6);
        assert_eq!(expanded_response.source_metadata.len(), 6);
        assert_eq!(
            expanded_response
                .hits
                .iter()
                .map(|hit| hit.uid)
                .collect::<Vec<_>>(),
            (0_u128..6)
                .map(|index| Uuid::from_u128(0x900 + index))
                .collect::<Vec<_>>()
        );
        assert!(
            expanded_response.source_refs.len() < expanded_response.hits.len(),
            "the tight evidence budget should render only a ranked prefix"
        );
        assert_eq!(
            expanded_response
                .source_refs
                .iter()
                .map(|source| source.source_uid.expect("graph source uid"))
                .collect::<Vec<_>>(),
            expanded_response
                .hits
                .iter()
                .take(expanded_response.source_refs.len())
                .map(|hit| hit.uid)
                .collect::<Vec<_>>()
        );
        assert!(expanded_response.consumed_evidence_tokens <= 256);
    }

    pub(super) fn verify_evidence_ranked_depth_validation() {
        // Pins: benchmark depth is bounded to the official LongMemEval maximum.
        for invalid in [0, 51] {
            let error = MemoryEvidenceRequest::new("ranked evidence", 256)
                .with_ranked_occurrence_depth(invalid)
                .expect_err("out-of-range ranked occurrence depth must fail");
            assert!(matches!(
                error,
                moa_core::error::MoaError::ValidationError(ref message)
                    if message == "memory evidence ranked occurrence depth must be in 1..=50"
            ));
        }
        assert_eq!(
            MemoryEvidenceRequest::new("ranked evidence", 256).ranked_occurrence_depth(),
            4
        );
    }

    pub(super) async fn verify_evidence_disabled_policy_returns_empty_without_retrieval() {
        // Pins: the public seam cannot bypass an agent policy that disables
        // memory, and it performs no backend work in that state.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retriever = scripted_graph_memory_retriever(calls.clone(), HashMap::new());
        let mut session = tenant_only_session();
        session.agent_context = Some(agent_context_with_knowledge_policy(AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        }));
        let ctx = WorkingContext::new(&session, capabilities());

        let response = retriever
            .retrieve_evidence(
                &ctx,
                MemoryEvidenceRequest::new("What is my rotation policy?", 256),
            )
            .await
            .expect("disabled policy should return empty evidence");

        assert!(response.hits.is_empty());
        assert!(response.rendered_evidence.is_empty());
        assert_eq!(response.consumed_evidence_tokens, 0);
        assert!(response.source_refs.is_empty());
        assert!(response.source_metadata.is_empty());
        assert_eq!(
            calls.lock().expect("scripted retriever calls lock").len(),
            0,
            "disabled memory policy must not touch retrieval"
        );
    }

    fn scripted_graph_memory_retriever(
        calls: Arc<Mutex<Vec<RecordedRetrievalRequest>>>,
        hits_by_scope: HashMap<MemoryScope, Vec<crate::retrieval::RetrievalHit>>,
    ) -> GraphMemoryRetriever {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let retriever = Arc::new(ScriptedPlannedRetriever {
            calls,
            hits_by_scope,
        });
        GraphMemoryRetriever::new(pool, None)
            .with_scoped_runtime_factory(Arc::new(ScriptedRuntimeFactory { retriever }))
    }

    fn agent_context_with_knowledge_policy(policy: AgentKnowledgePolicy) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            knowledge_policy: policy,
            ..AgentPolicySnapshot::default()
        };
        let mut context = AgentContext::system_default();
        context.policy_snapshot =
            serde_json::to_value(snapshot).expect("policy snapshot should serialize");
        context
    }

    #[allow(clippy::too_many_arguments)]
    fn retrieval_hit(
        uid: Uuid,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        label: NodeLabel,
        scope: &str,
        source_tier: crate::retrieval::SourceTier,
        name: &str,
        summary: &str,
        score: f64,
    ) -> crate::retrieval::RetrievalHit {
        crate::retrieval::RetrievalHit {
            uid,
            score,
            legs: crate::retrieval::LegSources {
                graph: false,
                vector: false,
                lexical: true,
            },
            similarity: None,
            lexical_backend: Some(crate::retrieval::LexicalBackend::PostgresTsvector),
            source_tier,
            knowledge_chunk: None,
            node: NodeIndexRow {
                uid,
                label,
                storage_partition_id: Some(tenant_id.to_string()),
                contact_id: contact_id.map(|id| id.to_string()),
                scope: scope.to_string(),
                name: name.to_string(),
                pii_class: PiiClass::None,
                valid_to: None,
                valid_from: Utc::now(),
                properties_summary: Some(json!({ "summary": summary })),
                last_accessed_at: Utc::now(),
                quality_score: 0.5,
            },
        }
    }

    fn section_between<'a>(content: &'a str, start: &str, end: &str) -> &'a str {
        let start_index = content
            .find(start)
            .expect("section start marker should exist")
            + start.len();
        let end_index = content[start_index..]
            .find(end)
            .expect("section end marker should exist")
            + start_index;
        &content[start_index..end_index]
    }

    fn contact_session(
        contact_id: ContactId,
        state: ContactVerificationState,
        linked_contact_ids: Vec<ContactId>,
    ) -> SessionMeta {
        let tenant_id = TenantId::new();
        SessionMeta {
            id: SessionId::new(),
            tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("mock"),
            contact: Some(ContactRef {
                contact_id,
                tenant_id,
                state,
                canonical_contact_id: None,
                linked_contact_ids,
                scopes: Vec::new(),
                permissions: serde_json::Value::Null,
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            ..SessionMeta::default()
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("mock"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }
}

#[cfg(test)]
mod evidence {
    #[tokio::test]
    async fn rejects_zero_token_budget_before_retrieval() {
        // Pins: the public evidence seam rejects a zero budget before retrieval.
        super::tests::verify_evidence_rejects_zero_token_budget_before_retrieval().await;
    }

    #[tokio::test]
    async fn returns_only_admitted_hits_with_aligned_typed_sources() {
        // Pins: returned hits obey production admission and align with typed provenance.
        super::tests::verify_evidence_returns_only_admitted_hits_with_aligned_typed_sources().await;
    }

    #[tokio::test]
    async fn token_budget_caps_exact_rendered_output() {
        // Pins: exact rendered evidence never exceeds its explicit token budget.
        super::tests::verify_evidence_token_budget_caps_exact_rendered_output().await;
    }

    #[tokio::test]
    async fn disabled_policy_returns_empty_without_retrieval() {
        // Pins: disabled memory policy cannot be bypassed through the public seam.
        super::tests::verify_evidence_disabled_policy_returns_empty_without_retrieval().await;
    }
}

#[cfg(test)]
mod evidence_ranked {
    #[tokio::test]
    async fn evidence_ranked_request_local_depth_preserves_hits_before_rendering() {
        // Pins: ranked retrieval depth and rendered reader evidence are independent.
        super::tests::verify_evidence_ranked_depth_is_request_local_and_rendering_is_separate()
            .await;
    }

    #[test]
    fn evidence_ranked_validates_depth_and_preserves_stage_seven_default() {
        // Pins: stage 7 remains four while eval depth is explicitly bounded.
        super::tests::verify_evidence_ranked_depth_validation();
    }
}
