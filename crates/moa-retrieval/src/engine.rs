//! Shared, policy-aware memory retrieval engine.
//!
//! The engine is the single owner of query routing, scoped runtime reuse,
//! planning, cache probing, embedding degradation, backend execution, admission,
//! and cross-scope fusion. Callers provide only authenticated policy inputs and
//! adapt the returned hits to their presentation or transport surface.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::try_join_all;
use moa_config::MoaConfig;
use moa_core::error::{MoaError, Result};
use moa_core::traits::EmbeddingProvider;
use moa_core::types::contact::ContactId;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{InformationBarrierClearances, RlsContext, SourceAclContext};
use moa_core::types::security::SensitivityClass;
use moa_crypto::KeyManagementProvider;
use moa_memory_graph::{GraphStore, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{QueryEmbedding, VectorStoreFactory};
use sqlx::PgPool;

use crate::planning::{PlannedQuery, PlanningCtx, QueryPlanner, Strategy};
use crate::retrieval::types::RetrievalLeg;
use crate::retrieval::{
    CachedHybridRetriever, EvidenceWindowPolicy, HybridRetriever, LineageContext,
    MemoryAdmissionPolicy, PlannedRetriever, RetrievalHit, RetrievalOutput, RetrievalProvenance,
    RetrievalRequest, RetrievalScopePlan, RetrievalStrategy, SourceTier, decompose_query,
    dedupe_and_rank_hits, route_query,
};

const SCOPED_RUNTIME_CACHE_CAPACITY: u64 = 512;
const SCOPED_RUNTIME_CACHE_TTL: Duration = Duration::from_secs(300);

/// Embedding identity attached to retrieval lineage by presentation adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingProvenance {
    /// Resolved embedding model identifier.
    pub model: String,
    /// Resolved embedding dimensionality.
    pub dimensions: u16,
}

/// Authenticated inputs for one policy-aware retrieval.
pub struct MemoryRetrievalRequest<'a> {
    query: &'a str,
    policy: &'a MemoryAdmissionPolicy,
    clearances: InformationBarrierClearances,
    result_limit: usize,
    lineage: Option<LineageContext>,
}

impl<'a> MemoryRetrievalRequest<'a> {
    /// Creates a retrieval request from authenticated policy inputs.
    #[must_use]
    pub fn new(
        query: &'a str,
        policy: &'a MemoryAdmissionPolicy,
        clearances: InformationBarrierClearances,
        result_limit: usize,
    ) -> Self {
        Self {
            query,
            policy,
            clearances,
            result_limit,
            lineage: None,
        }
    }

    /// Attaches replay-stable storage-lineage context for a production turn.
    #[must_use]
    pub fn with_lineage(mut self, lineage: LineageContext) -> Self {
        self.lineage = Some(lineage);
        self
    }
}

/// Result of one routed, admitted retrieval.
#[derive(Debug)]
pub struct MemoryRetrievalResult {
    /// Strategy selected by the query router.
    pub strategy: RetrievalStrategy,
    /// Fused and admitted hits in final rank order.
    pub hits: Vec<RetrievalHit>,
    /// Backend diagnostics retained for durable lineage emission.
    pub provenance: RetrievalProvenance,
    /// End-to-end engine time, excluding caller-specific rendering and auditing.
    pub elapsed: Duration,
}

/// Runtime backends for one graph-memory scope.
pub struct ScopedRetrievalRuntime {
    graph: Arc<dyn GraphStore>,
    retriever: Arc<dyn PlannedRetriever>,
}

impl ScopedRetrievalRuntime {
    /// Creates a scoped runtime from graph planning and retrieval backends.
    #[must_use]
    pub fn new(graph: Arc<dyn GraphStore>, retriever: Arc<dyn PlannedRetriever>) -> Self {
        Self { graph, retriever }
    }
}

/// Factory for scope-bound graph and retrieval backends.
#[async_trait]
pub trait ScopedRetrievalRuntimeFactory: Send + Sync {
    /// Builds one runtime for an RLS memory scope.
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &MoaConfig,
        pool: &PgPool,
        assume_app_role: bool,
    ) -> Result<ScopedRetrievalRuntime>;
}

/// Resolver for the durable source principals attached once per retrieval.
#[async_trait]
pub trait SourceAclContextResolver: Send + Sync {
    /// Resolves provider-source principals for an authenticated tenant/contact.
    async fn resolve(
        &self,
        pool: &PgPool,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        assume_app_role: bool,
    ) -> Result<SourceAclContext>;
}

struct DurableSourceAclContextResolver;

#[async_trait]
impl SourceAclContextResolver for DurableSourceAclContextResolver {
    async fn resolve(
        &self,
        pool: &PgPool,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        assume_app_role: bool,
    ) -> Result<SourceAclContext> {
        moa_db::resolve_source_acl_context(pool, tenant_id, contact_id, assume_app_role).await
    }
}

struct PostgresScopedRetrievalRuntimeFactory {
    kms: Arc<dyn KeyManagementProvider>,
    enrichment: Option<crate::retrieval::enrichment::EnrichmentHandle>,
}

#[async_trait]
impl ScopedRetrievalRuntimeFactory for PostgresScopedRetrievalRuntimeFactory {
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        config: &MoaConfig,
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
            PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context, self.kms.clone())
        } else {
            PostgresGraphStore::scoped(pool.clone(), scope_context, self.kms.clone())
        };
        let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
        let hybrid = Arc::new(
            HybridRetriever::from_config(config, pool.clone(), graph.clone(), pgvector_source)
                .with_assume_app_role(assume_app_role)
                .with_enrichment(self.enrichment.clone()),
        );
        let retriever: Arc<dyn PlannedRetriever> = if assume_app_role {
            Arc::new(CachedHybridRetriever::new_for_app_role(
                hybrid,
                pool.clone(),
            ))
        } else {
            Arc::new(CachedHybridRetriever::new(hybrid, pool.clone()))
        };
        Ok(ScopedRetrievalRuntime::new(graph, retriever))
    }
}

struct ScopeProbe<'a> {
    plan: &'a RetrievalScopePlan,
    planned: PlannedQuery,
    runtime: Arc<ScopedRetrievalRuntime>,
    cached_hits: Option<Vec<RetrievalHit>>,
}

/// Shared policy-aware graph-memory retrieval engine.
#[derive(Clone)]
pub struct MemoryRetrievalEngine {
    pool: PgPool,
    config: MoaConfig,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    assume_app_role: bool,
    planner: QueryPlanner,
    runtimes: moka::future::Cache<MemoryScope, Arc<ScopedRetrievalRuntime>>,
    runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
    source_acl_resolver: Arc<dyn SourceAclContextResolver>,
}

impl MemoryRetrievalEngine {
    /// Creates the production engine and its bounded scoped-runtime cache.
    #[must_use]
    pub fn new(
        config: MoaConfig,
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        let enrichment = tokio::runtime::Handle::try_current()
            .ok()
            .map(|_| crate::retrieval::enrichment::spawn_enrichment_worker(pool.clone()));
        Self {
            pool,
            config,
            embedder,
            assume_app_role: false,
            planner: QueryPlanner::new(),
            runtimes: scoped_runtime_cache(),
            runtime_factory: Arc::new(PostgresScopedRetrievalRuntimeFactory { kms, enrichment }),
            source_acl_resolver: Arc::new(DurableSourceAclContextResolver),
        }
    }

    /// Configures owner-role tests to assume the production application role.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Replaces the scope-runtime factory and clears cached runtimes.
    #[must_use]
    pub fn with_runtime_factory(
        mut self,
        runtime_factory: Arc<dyn ScopedRetrievalRuntimeFactory>,
    ) -> Self {
        self.runtime_factory = runtime_factory;
        self.runtimes = scoped_runtime_cache();
        self
    }

    /// Replaces the durable source-ACL resolver.
    #[must_use]
    pub fn with_source_acl_resolver(mut self, resolver: Arc<dyn SourceAclContextResolver>) -> Self {
        self.source_acl_resolver = resolver;
        self
    }

    /// Returns whether the engine has a vector embedding provider.
    #[must_use]
    pub fn has_vector_retrieval(&self) -> bool {
        self.embedder.is_some()
    }

    /// Returns the resolved embedding identity used by lineage adapters.
    #[must_use]
    pub fn embedding_provenance(&self) -> EmbeddingProvenance {
        match self.embedder.as_deref() {
            Some(embedder) => EmbeddingProvenance {
                model: embedder.model_id().to_string(),
                dimensions: embedder.dimensions().min(u16::MAX as usize) as u16,
            },
            None => EmbeddingProvenance {
                model: self.config.memory.embedding_model.clone(),
                dimensions: moa_memory_vector::VECTOR_DIMENSION as u16,
            },
        }
    }

    /// Routes and executes one authenticated retrieval through the shared engine.
    pub async fn retrieve(
        &self,
        request: MemoryRetrievalRequest<'_>,
    ) -> Result<MemoryRetrievalResult> {
        let strategy = route_query(request.query);
        if strategy == RetrievalStrategy::Skip || !request.policy.is_enabled() {
            return Ok(MemoryRetrievalResult {
                strategy,
                hits: Vec::new(),
                provenance: RetrievalProvenance::default(),
                elapsed: Duration::ZERO,
            });
        }

        let source_acl = self
            .source_acl_resolver
            .resolve(
                &self.pool,
                request.policy.tenant_id(),
                request.policy.contact_id(),
                self.assume_app_role,
            )
            .await?;
        let policy = request.policy.clone().with_source_acl(source_acl);
        let result_limit = policy.result_limit(request.result_limit);
        let started = Instant::now();
        let (hits, provenance) = match strategy {
            RetrievalStrategy::Deep => {
                self.retrieve_deep(
                    request.query,
                    &policy,
                    &request.clearances,
                    result_limit,
                    request.lineage.as_ref(),
                )
                .await?
            }
            RetrievalStrategy::Fast | RetrievalStrategy::Agentic => {
                self.retrieve_hits(
                    request.query,
                    &policy,
                    &request.clearances,
                    result_limit,
                    request.lineage.as_ref(),
                )
                .await?
            }
            RetrievalStrategy::Skip => (Vec::new(), RetrievalProvenance::default()),
        };
        Ok(MemoryRetrievalResult {
            strategy,
            hits,
            provenance,
            elapsed: started.elapsed(),
        })
    }

    async fn retrieve_deep(
        &self,
        query: &str,
        policy: &MemoryAdmissionPolicy,
        clearances: &InformationBarrierClearances,
        result_limit: usize,
        lineage: Option<&LineageContext>,
    ) -> Result<(Vec<RetrievalHit>, RetrievalProvenance)> {
        let mut sub_queries = decompose_query(query);
        if sub_queries.is_empty() {
            sub_queries.push(query.to_string());
        }
        let results = try_join_all(sub_queries.iter().map(|sub_query| {
            self.retrieve_hits(sub_query, policy, clearances, result_limit, lineage)
        }))
        .await?;
        let mut hits = Vec::new();
        let mut provenance = RetrievalProvenance::default();
        for (sub_hits, sub_provenance) in results {
            hits.extend(sub_hits);
            provenance.merge(sub_provenance);
        }
        Ok((dedupe_and_rank_hits(hits, result_limit), provenance))
    }

    async fn retrieve_hits(
        &self,
        query: &str,
        policy: &MemoryAdmissionPolicy,
        clearances: &InformationBarrierClearances,
        result_limit: usize,
        lineage: Option<&LineageContext>,
    ) -> Result<(Vec<RetrievalHit>, RetrievalProvenance)> {
        if policy.plans().is_empty() {
            return Ok((Vec::new(), RetrievalProvenance::default()));
        }
        let max_pii_class = policy.max_pii_class()?;
        let probes = try_join_all(policy.plans().iter().map(|plan| {
            self.probe_scope(query, plan, policy, clearances, max_pii_class, result_limit)
        }))
        .await?;
        let query_embedding = if probes.iter().any(|probe| probe.cached_hits.is_none()) {
            self.query_embedding(query).await
        } else {
            None
        };
        let scope_results = try_join_all(probes.iter().map(|probe| {
            let query_embedding = query_embedding.clone();
            async move {
                match &probe.cached_hits {
                    Some(hits) => Ok::<_, MoaError>((hits.clone(), RetrievalProvenance::default())),
                    None => {
                        let request = self.build_scope_request(
                            query,
                            query_embedding,
                            probe.plan,
                            policy,
                            clearances,
                            &probe.planned,
                            max_pii_class,
                            result_limit,
                            lineage,
                        );
                        let output = probe
                            .runtime
                            .retriever
                            .retrieve(&probe.planned, request)
                            .await
                            .map_err(retrieval_error)?;
                        Ok(provenance_from_output(output))
                    }
                }
            }
        }))
        .await?;

        let mut hits = Vec::new();
        let mut provenance = RetrievalProvenance::default();
        for (probe, (scope_hits, scope_provenance)) in probes.iter().zip(scope_results) {
            provenance.merge(scope_provenance);
            let before = scope_hits.len();
            let admitted = scope_hits
                .into_iter()
                .filter_map(|hit| policy.admit_hit(hit, probe.plan))
                .collect::<Vec<_>>();
            provenance.admission_rejected = provenance
                .admission_rejected
                .saturating_add(before.saturating_sub(admitted.len()));
            hits.extend(admitted);
        }
        Ok((dedupe_and_rank_hits(hits, result_limit), provenance))
    }

    async fn probe_scope<'a>(
        &self,
        query: &str,
        plan: &'a RetrievalScopePlan,
        policy: &MemoryAdmissionPolicy,
        clearances: &InformationBarrierClearances,
        max_pii_class: SensitivityClass,
        result_limit: usize,
    ) -> Result<ScopeProbe<'a>> {
        let runtime = self.runtime_for_scope(plan.scope()).await?;
        let planning = PlanningCtx::new(plan.scope().clone(), runtime.graph.clone());
        let planned = self.planner.plan(query, &planning).await.map_err(|error| {
            MoaError::StorageError(format!("graph memory planning failed: {error}"))
        })?;
        let request = self.build_scope_request(
            query,
            None,
            plan,
            policy,
            clearances,
            &planned,
            max_pii_class,
            result_limit,
            None,
        );
        let cached_hits = runtime
            .retriever
            .retrieve_cached(&planned, &request)
            .await
            .map_err(retrieval_error)?;
        Ok(ScopeProbe {
            plan,
            planned,
            runtime,
            cached_hits,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn build_scope_request(
        &self,
        query: &str,
        query_embedding: Option<QueryEmbedding>,
        plan: &RetrievalScopePlan,
        policy: &MemoryAdmissionPolicy,
        clearances: &InformationBarrierClearances,
        planned: &PlannedQuery,
        max_pii_class: SensitivityClass,
        result_limit: usize,
        lineage: Option<&LineageContext>,
    ) -> RetrievalRequest {
        let mut request = planned.clone().into_retrieval_request(
            query.to_string(),
            query_embedding,
            max_pii_class,
            result_limit,
            true,
        );
        let ranking = &self.config.memory.retrieval.ranking;
        request.window_policy = EvidenceWindowPolicy {
            rerank_window: ranking.rerank_window,
            abstain_below_window_evidence: ranking.abstain_below_window_evidence,
        };
        request.source_acl = policy.source_acl().clone();
        request.cleared_barriers = clearances.clone();
        request.label_filter = plan
            .label_filter()
            .map(<[moa_memory_graph::NodeLabel]>::to_vec);
        request.disable_graph_expansion = plan.source_tier() == SourceTier::TenantKnowledge;
        if plan.source_tier() == SourceTier::TenantKnowledge {
            request.strategy = Some(Strategy::VectorFirst);
        }
        request.lineage = lineage.cloned();
        request
    }

    async fn query_embedding(&self, query: &str) -> Option<QueryEmbedding> {
        let embedder = self.embedder.as_deref()?;
        let embedding = match embedder.embed(&[query.to_string()]).await {
            Ok(mut embeddings) => embeddings.pop().unwrap_or_default(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "query embedding failed; degrading to lexical-only retrieval"
                );
                metrics::counter!(
                    "moa_retrieval_leg_degraded_total",
                    "leg" => RetrievalLeg::Embedding.as_str(),
                    "reason" => "error",
                )
                .increment(1);
                return None;
            }
        };
        match QueryEmbedding::new(embedding, embedder.model_id()) {
            Ok(embedding) => Some(embedding),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "query embedding was invalid; degrading to lexical-only retrieval"
                );
                None
            }
        }
    }

    async fn runtime_for_scope(&self, scope: &MemoryScope) -> Result<Arc<ScopedRetrievalRuntime>> {
        self.runtimes
            .try_get_with(scope.clone(), async {
                self.runtime_factory
                    .build_runtime(scope, &self.config, &self.pool, self.assume_app_role)
                    .await
                    .map(Arc::new)
            })
            .await
            .map_err(|error| {
                MoaError::StorageError(format!(
                    "scoped retrieval runtime initialization failed: {error}"
                ))
            })
    }
}

fn scoped_runtime_cache() -> moka::future::Cache<MemoryScope, Arc<ScopedRetrievalRuntime>> {
    moka::future::Cache::builder()
        .max_capacity(SCOPED_RUNTIME_CACHE_CAPACITY)
        .time_to_live(SCOPED_RUNTIME_CACHE_TTL)
        .build()
}

fn provenance_from_output(mut output: RetrievalOutput) -> (Vec<RetrievalHit>, RetrievalProvenance) {
    let mut provenance = output.provenance;
    provenance.graph_paths = std::mem::take(&mut output.diagnostics.path_traces);
    (output.hits, provenance)
}

fn retrieval_error(error: crate::retrieval::RetrievalError) -> MoaError {
    MoaError::StorageError(format!("graph memory retrieval failed: {error}"))
}
