//! Production hybrid graph-memory retriever.
//!
//! This remains one module because `HybridRetriever` owns the graph, vector,
//! and reranker boundary while individual retrieval legs live in `legs`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::MoaConfig;
use moa_core::RlsContext;
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, NodeIndexRow, NodeLabel};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{Error as VectorError, PgvectorStore, TurbopufferStore};
use moa_providers::{ConfiguredReranker, Reranker, build_reranker_from_config};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::graph_seed::{
    GraphSeedPlan, hydrate_graph_seed_rows, interim_graph_seed_plan, semantic_entity_seed_uids,
};
use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, LegCandidate, RRF_K, VECTOR_BUDGET,
    VECTOR_WEIGHT, bump_last_accessed, graph_expansion_leg_with_diagnostics, hydrate_nodes,
    lexical_leg, rrf_fuse, timed_leg, turbopuffer_bm25_leg, vector_leg as run_vector_leg,
    write_retrieval_lineage,
};
use crate::retrieval::policy::{GraphRetrievalPolicy, effective_graph_policy};
use crate::retrieval::ranking::{
    FeatureRanker, RankingConfig, normalize_tokens, ranking_fingerprint,
};
use crate::retrieval::source_rank::{
    apply_source_object_graph_ranking, select_final_hits_for_policy,
};
use crate::retrieval::types::{
    GraphCandidateCounts, GraphRetrievalDiagnostics, KnowledgeChunkHydration, LegSources,
    LexicalBackend, Result, RetrievalError, RetrievalHit, RetrievalOutput, RetrievalRequest,
    SourceTier,
};

const MIN_FUSED_CANDIDATE_LIMIT: usize = 26;
const MAX_FUSED_CANDIDATE_LIMIT: usize = 100;
const FUSED_CANDIDATE_MULTIPLIER: usize = 2;
const TURBOPUFFER_BM25_BOOST_MULTIPLIER: f64 = 0.10;

/// Hybrid retriever that fuses graph, vector, and lexical retrieval.
#[derive(Clone)]
pub struct HybridRetriever {
    pool: PgPool,
    graph: Arc<dyn GraphStore>,
    pgvector_source: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
    reranker: Arc<dyn Reranker>,
    rerank_model: String,
    ranking_config: RankingConfig,
    assume_app_role: bool,
    lineage_enabled: bool,
    graph_policy: GraphRetrievalPolicy,
}

impl HybridRetriever {
    /// Creates a hybrid retriever with deterministic no-op reranking.
    #[must_use]
    pub fn new(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        pgvector_source: Arc<PgvectorStore>,
    ) -> Self {
        let configured_reranker = ConfiguredReranker::noop();
        Self {
            pool,
            graph,
            pgvector_source,
            turbopuffer: None,
            reranker: configured_reranker.reranker,
            rerank_model: configured_reranker.model,
            ranking_config: RankingConfig::default(),
            assume_app_role: false,
            lineage_enabled: false,
            graph_policy: GraphRetrievalPolicy::default(),
        }
    }

    /// Creates a hybrid retriever from shared config.
    #[must_use]
    pub fn from_config(
        config: &MoaConfig,
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        pgvector_source: Arc<PgvectorStore>,
    ) -> Self {
        let configured = configured_reranker_or_noop(config);
        let turbopuffer = TurbopufferStore::from_config(config).ok().map(Arc::new);
        Self::new(pool, graph, pgvector_source)
            .with_turbopuffer(turbopuffer)
            .with_configured_reranker(configured)
            .with_ranking_config(RankingConfig::from(&config.memory.retrieval.ranking))
            .with_lineage_enabled(config.memory.retrieval.lineage_enabled)
    }

    /// Adds an optional Turbopuffer target backend for promoted storage partitions.
    #[must_use]
    pub fn with_turbopuffer(mut self, turbopuffer: Option<Arc<TurbopufferStore>>) -> Self {
        self.turbopuffer = turbopuffer;
        self
    }

    /// Overrides the reranker backend.
    #[must_use]
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Overrides the reranker backend and model from provider configuration.
    #[must_use]
    pub fn with_configured_reranker(mut self, configured: ConfiguredReranker) -> Self {
        self.reranker = configured.reranker;
        self.rerank_model = configured.model;
        self
    }

    /// Overrides the reranker model while preserving the configured backend.
    #[must_use]
    pub fn with_rerank_model(mut self, model: impl Into<String>) -> Self {
        self.rerank_model = model.into();
        self
    }

    /// Overrides the deterministic post-hydration ranking configuration.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
        self
    }

    /// Overrides the graph retrieval policy used when requests do not explicitly disable graph expansion.
    #[must_use]
    pub fn with_graph_policy(mut self, graph_policy: GraphRetrievalPolicy) -> Self {
        self.graph_policy = graph_policy;
        self
    }

    /// Enables or disables fire-and-forget retrieval lineage sidecar writes.
    #[must_use]
    pub fn with_lineage_enabled(mut self, enabled: bool) -> Self {
        self.lineage_enabled = enabled;
        self
    }

    /// Returns the cache fingerprint for the configured ranking stage.
    #[must_use]
    pub fn ranking_fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&ranking_fingerprint(&self.ranking_config));
        hasher.update(self.graph_policy.as_str().as_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Assumes the `moa_app` role inside sidecar transactions.
    ///
    /// This is intended for integration tests that connect through the local owner role.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Retrieves graph-memory candidates through graph, vector, and lexical legs.
    pub async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>> {
        Ok(self.retrieve_with_diagnostics(req).await?.hits)
    }

    /// Retrieves graph-memory candidates and request-local diagnostics.
    pub async fn retrieve_with_diagnostics(
        &self,
        req: RetrievalRequest,
    ) -> Result<RetrievalOutput> {
        let graph_policy = effective_graph_policy(self.graph_policy, req.disable_graph_expansion);
        let mut diagnostics = GraphRetrievalDiagnostics::new(graph_policy);
        if req.k_final == 0 {
            return Ok(RetrievalOutput {
                hits: Vec::new(),
                diagnostics,
            });
        }

        let strategy = req.strategy.unwrap_or(Strategy::Both);
        let backend_state = if retrieval_needs_backend_state(&req) {
            self.vector_backend_state(&req).await?
        } else {
            VectorBackendState::default()
        };
        let vector_future = run_leg(
            req.disable_leg_timeouts,
            "vector",
            VECTOR_BUDGET,
            self.vector_leg(&req, &backend_state),
        );
        let lexical_future = run_leg(
            req.disable_leg_timeouts,
            "lexical",
            LEXICAL_BUDGET,
            self.lexical_leg(&req, &backend_state),
        );
        let (vector_hits, lexical_hits) = tokio::join!(vector_future, lexical_future);
        let mut vector_hits = vector_hits?;
        let mut lexical_hits = lexical_hits?;
        apply_lexical_boost_only_policy(&req, &vector_hits, &mut lexical_hits);
        let interim = rrf_fuse(
            &[],
            &vector_hits,
            &lexical_hits.candidates,
            weights_for(strategy),
        );
        let semantic_entity_seed_uids = if graph_policy.allows_semantic_entity_seeds() {
            semantic_entity_seed_uids(&self.pool, &req, self.assume_app_role).await?
        } else {
            Vec::new()
        };
        let graph_seed_rows = if graph_policy.disables_graph_ranking() {
            Vec::new()
        } else {
            hydrate_graph_seed_rows(
                &self.pool,
                &req,
                &interim,
                &semantic_entity_seed_uids,
                self.assume_app_role,
            )
            .await?
        };
        let graph_seed_plan = if graph_policy.disables_graph_ranking() {
            GraphSeedPlan::default()
        } else {
            interim_graph_seed_plan(
                graph_policy,
                &req.seeds,
                &semantic_entity_seed_uids,
                &interim,
                &graph_seed_rows,
                &req.query_text,
            )
        };
        diagnostics.seed_counts = graph_seed_plan.seed_counts;
        let graph_output = if graph_seed_plan.strengths.is_empty() {
            Default::default()
        } else {
            let graph_started = std::time::Instant::now();
            let mut output = run_leg(
                req.disable_leg_timeouts,
                "graph",
                GRAPH_BUDGET,
                graph_expansion_leg_with_diagnostics(
                    self.graph.as_ref(),
                    &req,
                    &graph_seed_plan.strengths,
                    &graph_seed_rows,
                    &graph_seed_plan.seed_sources,
                    graph_policy,
                ),
            )
            .await?;
            output.diagnostics.graph_latency_ms = duration_ms_u64(graph_started.elapsed());
            output
        };
        diagnostics.graph_latency_ms = graph_output.diagnostics.graph_latency_ms;
        diagnostics.record_paths(graph_output.diagnostics);
        let graph_hits = if graph_policy.uses_graph_candidate_fusion() {
            graph_output.candidates
        } else {
            Vec::new()
        };
        let fusion_started = std::time::Instant::now();
        let mut fused = rrf_fuse(
            &graph_hits,
            &vector_hits,
            &lexical_hits.candidates,
            weights_for(strategy),
        );
        if fused.is_empty()
            && should_retry_vector_after_empty_fusion(
                &req,
                &vector_hits,
                &lexical_hits.candidates,
                &graph_hits,
            )
        {
            vector_hits = self.vector_leg(&req, &backend_state).await?;
            fused = rrf_fuse(
                &graph_hits,
                &vector_hits,
                &lexical_hits.candidates,
                weights_for(strategy),
            );
        }
        fused.truncate(fused_candidate_limit(req.k_final));
        diagnostics.candidate_counts = graph_candidate_counts(&fused);
        if fused.is_empty() {
            return Ok(RetrievalOutput {
                hits: Vec::new(),
                diagnostics,
            });
        }

        let fused_uids = fused.iter().map(|(uid, _, _)| *uid).collect::<Vec<_>>();
        let nodes = hydrate_nodes(
            &self.pool,
            &req.scope,
            &fused_uids,
            self.assume_app_role,
            req.as_of,
        )
        .await?;
        let mut hits = build_hits(fused, nodes);
        annotate_lexical_backend(&mut hits, lexical_hits.backend);
        hydrate_knowledge_chunks(&self.pool, &req.scope, &mut hits, self.assume_app_role).await?;
        rank_hydrated_hits_for_policy(
            &mut hits,
            &self.ranking_config,
            &req,
            graph_policy,
            vector_hits.first().map(|candidate| candidate.uid),
        );
        if graph_policy.uses_source_object_ranking() {
            diagnostics.source_object_ranking = apply_source_object_graph_ranking(
                &mut hits,
                &req,
                &diagnostics.path_traces,
                vector_hits.first().map(|candidate| candidate.uid),
                graph_policy,
            );
        }
        let final_hits = if req.use_reranker && hits.len() > req.k_final {
            let reranked = self.rerank_hits(&req, &hits).await?;
            select_final_hits_for_policy(reranked, &hits, req.k_final, graph_policy)
        } else {
            select_final_hits_for_policy(hits, &[], req.k_final, graph_policy)
        };
        metrics::histogram!("moa_retrieval_rrf_rerank_seconds")
            .record(fusion_started.elapsed().as_secs_f64());

        if req.ranking_reference_time.is_none() {
            let touched_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
            let pool = self.pool.clone();
            let scope = req.scope.clone();
            let assume_app_role = self.assume_app_role;
            tokio::spawn(async move {
                if let Err(error) =
                    bump_last_accessed(pool, scope, touched_uids, assume_app_role).await
                {
                    tracing::debug!(error = %error, "failed to bump graph-memory access timestamps");
                }
            });
        }
        if self.lineage_enabled
            && let Some(lineage) = req.lineage
        {
            let ranked_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
            let pool = self.pool.clone();
            let scope = req.scope.clone();
            let assume_app_role = self.assume_app_role;
            let retrieved_at = Utc::now();
            tokio::spawn(async move {
                if let Err(error) = write_retrieval_lineage(
                    pool,
                    scope,
                    lineage,
                    ranked_uids,
                    retrieved_at,
                    assume_app_role,
                )
                .await
                {
                    tracing::debug!(error = %error, "failed to write graph-memory retrieval lineage");
                }
            });
        }

        Ok(RetrievalOutput {
            hits: final_hits,
            diagnostics,
        })
    }

    async fn vector_leg(
        &self,
        req: &RetrievalRequest,
        state: &VectorBackendState,
    ) -> Result<Vec<LegCandidate>> {
        if req.query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        if req.as_of.is_some() {
            return run_vector_leg(self.pgvector_source.as_ref(), req).await;
        }

        let tenant_id = req.scope.tenant_id();
        if state.is_dual_read_active() {
            return self.dual_read_vector_leg(req).await;
        }
        if state.vector_backend == "turbopuffer" {
            let turbopuffer = self
                .turbopuffer
                .as_ref()
                .ok_or_else(|| turbopuffer_unavailable_for_request(req))?;
            let scoped_turbopuffer = turbopuffer.scoped_to_tenant(tenant_id);
            return run_vector_leg(&scoped_turbopuffer, req).await;
        }

        run_vector_leg(self.pgvector_source.as_ref(), req).await
    }

    async fn lexical_leg(
        &self,
        req: &RetrievalRequest,
        state: &VectorBackendState,
    ) -> Result<LexicalLegOutput> {
        if state.uses_turbopuffer_backend()
            && req.as_of.is_none()
            && request_allows_tenant_chunk_bm25(req)
        {
            let turbopuffer = self
                .turbopuffer
                .as_ref()
                .ok_or_else(|| turbopuffer_unavailable_for_request(req))?;
            let scoped_turbopuffer = turbopuffer.scoped_to_tenant(req.scope.tenant_id());
            match turbopuffer_bm25_leg(&scoped_turbopuffer, req).await {
                Ok(hits) => {
                    if request_is_tenant_chunk_only(req) {
                        record_lexical_backend(LexicalBackend::TurbopufferBm25, "success");
                        return Ok(LexicalLegOutput::new(hits, LexicalBackend::TurbopufferBm25));
                    }
                    let postgres_hits = lexical_leg(&self.pool, req, self.assume_app_role).await?;
                    record_lexical_backend(LexicalBackend::Mixed, "success");
                    return Ok(LexicalLegOutput::new(
                        merge_lexical_candidates(hits, postgres_hits),
                        LexicalBackend::Mixed,
                    ));
                }
                Err(error) => {
                    record_lexical_backend(LexicalBackend::TurbopufferBm25, "fallback");
                    tracing::warn!(
                        error = %error,
                        tenant_id = %req.scope.tenant_id(),
                        "Turbopuffer BM25 lexical leg failed; falling back to Postgres lexical"
                    );
                }
            }
        }

        let hits = lexical_leg(&self.pool, req, self.assume_app_role).await?;
        record_lexical_backend(LexicalBackend::PostgresTsvector, "success");
        Ok(LexicalLegOutput::new(
            hits,
            LexicalBackend::PostgresTsvector,
        ))
    }

    async fn dual_read_vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<LegCandidate>> {
        let Some(turbopuffer) = &self.turbopuffer else {
            return Err(turbopuffer_unavailable_for_request(req));
        };

        let scoped_turbopuffer = turbopuffer.scoped_to_tenant(req.scope.tenant_id());
        let pg_future = run_vector_leg(self.pgvector_source.as_ref(), req);
        let tp_future = run_vector_leg(&scoped_turbopuffer, req);
        let (pg_result, tp_result) = tokio::join!(pg_future, tp_future);

        if let (Ok(pg_hits), Ok(tp_hits)) = (&pg_result, &tp_result) {
            metrics::histogram!("moa_vector_dualread_overlap")
                .record(leg_overlap(pg_hits, tp_hits, 10));
        }

        match (tp_result, pg_result) {
            (Ok(tp_hits), _) => Ok(tp_hits),
            (Err(error), Ok(pg_hits)) => {
                tracing::warn!(error = %error, "Turbopuffer vector dual-read leg failed; using pgvector result");
                Ok(pg_hits)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    async fn vector_backend_state(&self, req: &RetrievalRequest) -> Result<VectorBackendState> {
        let scope = req.scope.to_rls_context();
        let mut conn = ScopedConn::begin(&self.pool, &scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let row = sqlx::query_as::<_, (String, String, Option<DateTime<Utc>>)>(
            r#"
                SELECT vector_backend, vector_backend_state, dual_read_until
                FROM moa.storage_partition_state
                WHERE tenant_id = $1
                "#,
        )
        .bind(req.scope.tenant_id().0)
        .fetch_optional(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(row
            .map(
                |(vector_backend, vector_backend_state, dual_read_until)| VectorBackendState {
                    vector_backend,
                    vector_backend_state,
                    dual_read_until,
                },
            )
            .unwrap_or_default())
    }

    async fn rerank_hits(
        &self,
        req: &RetrievalRequest,
        hits: &[RetrievalHit],
    ) -> Result<Vec<RetrievalHit>> {
        let documents = hits.iter().map(rerank_document).collect::<Vec<_>>();
        let reranked = self
            .reranker
            .rerank(&self.rerank_model, &req.query_text, &documents, req.k_final)
            .await
            .map_err(|error| RetrievalError::Rerank(error.to_string()))?;
        let mut out = Vec::with_capacity(req.k_final.min(reranked.len()));
        for hit in reranked {
            if let Some(candidate) = hits.get(hit.index) {
                out.push(candidate.clone());
            }
        }
        if out.is_empty() {
            Ok(hits.iter().take(req.k_final).cloned().collect())
        } else {
            Ok(out)
        }
    }
}

fn fused_candidate_limit(k_final: usize) -> usize {
    if k_final == 0 {
        return 0;
    }
    k_final
        .saturating_mul(FUSED_CANDIDATE_MULTIPLIER)
        .clamp(MIN_FUSED_CANDIDATE_LIMIT, MAX_FUSED_CANDIDATE_LIMIT)
}

fn should_retry_vector_after_empty_fusion(
    req: &RetrievalRequest,
    vector_hits: &[LegCandidate],
    lexical_hits: &[LegCandidate],
    graph_hits: &[LegCandidate],
) -> bool {
    !req.disable_leg_timeouts
        && !req.query_embedding.is_empty()
        && vector_hits.is_empty()
        && lexical_hits.is_empty()
        && graph_hits.is_empty()
}

fn rerank_document(hit: &RetrievalHit) -> String {
    let Some(chunk) = &hit.knowledge_chunk else {
        return hit.node.name.clone();
    };
    let mut parts = Vec::new();
    if let Some(title) = chunk
        .source_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        parts.push(format!("Title: {title}"));
    }
    if !chunk.heading_path.is_empty() {
        parts.push(format!("Section: {}", chunk.heading_path.join(" > ")));
    }
    parts.push(chunk.text.clone());
    parts.join("\n")
}

fn configured_reranker_or_noop(config: &MoaConfig) -> ConfiguredReranker {
    build_reranker_from_config(config).unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "graph-memory reranking disabled because reranker configuration is invalid"
        );
        ConfiguredReranker::noop()
    })
}

#[derive(Debug, Clone)]
struct VectorBackendState {
    vector_backend: String,
    vector_backend_state: String,
    dual_read_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct LexicalLegOutput {
    candidates: Vec<LegCandidate>,
    backend: Option<LexicalBackend>,
}

impl LexicalLegOutput {
    fn new(candidates: Vec<LegCandidate>, backend: LexicalBackend) -> Self {
        Self {
            candidates,
            backend: Some(backend),
        }
    }
}

impl Default for VectorBackendState {
    fn default() -> Self {
        Self {
            vector_backend: "pgvector".to_string(),
            vector_backend_state: "steady".to_string(),
            dual_read_until: None,
        }
    }
}

impl VectorBackendState {
    fn is_dual_read_active(&self) -> bool {
        self.vector_backend_state == "dual_read"
            && self.dual_read_until.is_none_or(|until| until > Utc::now())
    }

    fn uses_turbopuffer_backend(&self) -> bool {
        self.vector_backend == "turbopuffer"
    }
}

fn retrieval_needs_backend_state(req: &RetrievalRequest) -> bool {
    !req.query_embedding.is_empty() || !req.query_text.trim().is_empty()
}

fn turbopuffer_unavailable_for_request(req: &RetrievalRequest) -> RetrievalError {
    VectorError::TurbopufferUnavailable {
        storage_partition_id: req
            .scope
            .to_rls_context()
            .storage_partition_id()
            .to_string(),
    }
    .into()
}

fn request_allows_tenant_chunk_bm25(req: &RetrievalRequest) -> bool {
    if req.strategy == Some(Strategy::VectorFirst) {
        return false;
    }
    req.label_filter
        .as_deref()
        .is_some_and(|labels| labels.contains(&NodeLabel::Chunk))
}

fn request_is_tenant_chunk_only(req: &RetrievalRequest) -> bool {
    req.label_filter
        .as_deref()
        .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn merge_lexical_candidates(
    primary: Vec<LegCandidate>,
    fallback: Vec<LegCandidate>,
) -> Vec<LegCandidate> {
    let mut seen = HashSet::new();
    primary
        .into_iter()
        .chain(fallback)
        .filter_map(|candidate| seen.insert(candidate.uid).then_some(candidate.uid))
        .enumerate()
        .map(|(index, uid)| LegCandidate {
            uid,
            score: 1.0 / (RRF_K + index as f64 + 1.0),
        })
        .collect()
}

fn record_lexical_backend(backend: LexicalBackend, outcome: &'static str) {
    metrics::counter!(
        "moa_retrieval_lexical_backend_total",
        "backend" => backend.as_str(),
        "outcome" => outcome
    )
    .increment(1);
}

fn apply_lexical_boost_only_policy(
    req: &RetrievalRequest,
    vector_hits: &[LegCandidate],
    lexical_hits: &mut LexicalLegOutput,
) {
    if lexical_hits.backend != Some(LexicalBackend::TurbopufferBm25)
        || !request_is_tenant_chunk_only(req)
        || vector_hits.is_empty()
    {
        return;
    }
    let vector_uids = vector_hits
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<HashSet<_>>();
    lexical_hits
        .candidates
        .retain(|candidate| vector_uids.contains(&candidate.uid));
    for candidate in &mut lexical_hits.candidates {
        candidate.score *= TURBOPUFFER_BM25_BOOST_MULTIPLIER;
    }
}

fn leg_overlap(left: &[LegCandidate], right: &[LegCandidate], k: usize) -> f64 {
    let left_set = left
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let right_set = right
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let denom = left_set.len().max(right_set.len()).max(1).min(k);
    left_set.intersection(&right_set).count() as f64 / denom as f64
}

fn weights_for(strategy: Strategy) -> (f64, f64, f64) {
    match strategy {
        Strategy::GraphFirst => (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT * 0.5),
        Strategy::VectorFirst | Strategy::Both => (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT),
    }
}

fn leg_or_empty<T>(
    name: &'static str,
    result: std::result::Result<Result<T>, tokio::time::error::Elapsed>,
) -> Result<T>
where
    T: Default,
{
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::debug!(leg = name, "hybrid retrieval leg exceeded budget");
            Ok(T::default())
        }
    }
}

async fn run_leg<T, F>(
    disable_timeout: bool,
    name: &'static str,
    budget: std::time::Duration,
    future: F,
) -> Result<T>
where
    T: Default,
    F: std::future::Future<Output = Result<T>>,
{
    if disable_timeout {
        return future.await;
    }
    leg_or_empty(name, timed_leg(name, budget, future).await)
}

fn build_hits(fused: Vec<(Uuid, f64, LegSources)>, nodes: Vec<NodeIndexRow>) -> Vec<RetrievalHit> {
    let mut nodes_by_uid = nodes
        .into_iter()
        .map(|node| (node.uid, node))
        .collect::<HashMap<_, _>>();
    fused
        .into_iter()
        .filter_map(|(uid, score, legs)| {
            nodes_by_uid.remove(&uid).map(|node| RetrievalHit {
                uid,
                score,
                legs,
                lexical_backend: None,
                source_tier: source_tier_for_node(&node),
                knowledge_chunk: None,
                node,
            })
        })
        .collect()
}

fn annotate_lexical_backend(hits: &mut [RetrievalHit], backend: Option<LexicalBackend>) {
    for hit in hits {
        if hit.legs.lexical {
            hit.lexical_backend = backend;
        }
    }
}

fn graph_candidate_counts(fused: &[(Uuid, f64, LegSources)]) -> GraphCandidateCounts {
    let mut counts = GraphCandidateCounts::default();
    for (_, _, sources) in fused {
        match (sources.graph, sources.vector, sources.lexical) {
            (true, false, false) => counts.graph_only += 1,
            (true, true, false) => counts.vector_graph += 1,
            (true, false, true) => counts.lexical_graph += 1,
            (true, true, true) => counts.all_legs += 1,
            _ => {}
        }
    }
    counts
}

async fn hydrate_knowledge_chunks(
    pool: &PgPool,
    scope: &MemoryScope,
    hits: &mut [RetrievalHit],
    assume_app_role: bool,
) -> Result<()> {
    let chunk_uids = hits
        .iter()
        .filter(|hit| hit.source_tier == SourceTier::TenantKnowledge)
        .filter(|hit| hit.node.label == NodeLabel::Chunk)
        .map(|hit| hit.uid)
        .collect::<Vec<_>>();
    if chunk_uids.is_empty() {
        return Ok(());
    }

    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(scope.tenant_id())).await?;
    if assume_app_role {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
    }
    let rows = sqlx::query_as::<_, KnowledgeChunkRow>(
        r#"
        SELECT DISTINCT ON (c.graph_node_uid)
            c.graph_node_uid,
            c.chunk_uid,
            c.document_version_id AS document_version_uid,
            v.object_id AS object_uid,
            c.chunk_hash,
            c.ordinal,
            c.heading_path,
            c.text,
            c.token_count,
            c.metadata,
            o.source_uri,
            o.title AS source_title,
            o.object_type
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        JOIN moa.knowledge_objects o
          ON o.object_uid = v.object_id
        WHERE c.tenant_id = $1
          AND c.graph_node_uid = ANY($2)
          AND o.status = 'active'
          AND c.metadata->>'active' IS DISTINCT FROM 'false'
        ORDER BY c.graph_node_uid, v.created_at DESC, c.ordinal ASC
        "#,
    )
    .bind(scope.tenant_id().0)
    .bind(&chunk_uids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;

    let mut chunks_by_graph_uid = rows
        .into_iter()
        .map(|row| {
            (
                row.graph_node_uid,
                KnowledgeChunkHydration {
                    chunk_uid: row.chunk_uid,
                    document_version_uid: row.document_version_uid,
                    object_uid: row.object_uid,
                    chunk_hash: row.chunk_hash,
                    ordinal: row.ordinal,
                    heading_path: row.heading_path,
                    text: row.text,
                    token_count: row.token_count,
                    metadata: row.metadata,
                    source_uri: row.source_uri,
                    source_title: row.source_title,
                    object_type: row.object_type,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for hit in hits {
        if let Some(chunk) = chunks_by_graph_uid.remove(&hit.uid) {
            hit.knowledge_chunk = Some(chunk);
        }
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct KnowledgeChunkRow {
    graph_node_uid: Uuid,
    chunk_uid: Uuid,
    document_version_uid: Uuid,
    object_uid: Uuid,
    chunk_hash: String,
    ordinal: i32,
    heading_path: Vec<String>,
    text: String,
    token_count: i32,
    metadata: Value,
    source_uri: Option<String>,
    source_title: Option<String>,
    object_type: String,
}

fn source_tier_for_node(node: &NodeIndexRow) -> SourceTier {
    if node.scope == "tenant" && is_tenant_knowledge_label(node.label) {
        SourceTier::TenantKnowledge
    } else {
        SourceTier::UserMemory
    }
}

fn is_tenant_knowledge_label(label: NodeLabel) -> bool {
    matches!(
        label,
        NodeLabel::Chunk | NodeLabel::Document | NodeLabel::ContactGroup
    )
}

#[cfg(test)]
fn rank_hydrated_hits(hits: &mut [RetrievalHit], config: &RankingConfig, req: &RetrievalRequest) {
    rank_hydrated_hits_for_policy(
        hits,
        config,
        req,
        GraphRetrievalPolicy::LegacyBroadExpansion,
        None,
    );
}

fn rank_hydrated_hits_for_policy(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
    graph_policy: GraphRetrievalPolicy,
    vector_rank_one: Option<Uuid>,
) {
    apply_feature_ranking(hits, config, req, graph_policy);
    preserve_vector_rank_one_for_policy(hits, graph_policy, vector_rank_one);
}

fn apply_feature_ranking(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
    graph_policy: GraphRetrievalPolicy,
) {
    let max_fused_score = hits.iter().map(|hit| hit.score).fold(0.0_f64, f64::max);
    let query_tokens = normalize_tokens(&req.query_text);
    let reference_time = req.ranking_reference_time.unwrap_or_else(Utc::now);
    let ranker = FeatureRanker::new(config, reference_time)
        .with_request_scope(&req.scope)
        .with_first_person_query(&req.query_text);
    for hit in hits.iter_mut() {
        let mut node = hit.node.clone();
        if let Some(chunk) = &hit.knowledge_chunk {
            node.name.clone_from(&chunk.text);
        }
        hit.score = ranker.score(hit.score, max_fused_score, &query_tokens, &node);
        if hit.legs.lexical && !hit.legs.vector && !hit.legs.graph {
            hit.score += config.weights.overlap;
        }
        if graph_policy.allows_graph_only_rescue_bonus()
            && hit.legs.graph
            && !hit.legs.vector
            && !hit.legs.lexical
        {
            hit.score += config.weights.graph_rescue;
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.uid.cmp(&right.uid))
    });
}

fn preserve_vector_rank_one_for_policy(
    hits: &mut [RetrievalHit],
    graph_policy: GraphRetrievalPolicy,
    vector_rank_one: Option<Uuid>,
) {
    if graph_policy != GraphRetrievalPolicy::AnchoredRescue || hits.len() < 2 {
        return;
    }
    let Some(vector_rank_one) = vector_rank_one else {
        return;
    };
    let Some(top) = hits.first() else {
        return;
    };
    if top.uid == vector_rank_one || !top.legs.graph || top.legs.vector || top.legs.lexical {
        return;
    }
    let Some(vector_index) = hits.iter().position(|hit| hit.uid == vector_rank_one) else {
        return;
    };
    hits[..=vector_index].rotate_right(1);
}

fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::TenantId;
    use moa_memory_graph::{GraphError, PiiClass};
    use moa_providers::{RerankHit, Reranker};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;
    use crate::retrieval::types::{GraphPathTrace, GraphSeedSource};

    fn tenant_scope() -> MemoryScope {
        MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::from_u128(0x100)),
        }
    }

    fn lazy_pgvector_source(pool: &PgPool) -> Arc<PgvectorStore> {
        Arc::new(PgvectorStore::new(
            pool.clone(),
            RlsContext::tenant(TenantId::from(Uuid::from_u128(0x100))),
        ))
    }

    #[tokio::test]
    async fn reranker_reorders_candidates_when_enabled() {
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        let retriever = HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
        .with_reranker(Arc::new(ReverseReranker));
        let req = RetrievalRequest {
            seeds: Vec::new(),
            query_text: "deploy provider".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: 1,
            use_reranker: true,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        };
        let first = hit(Uuid::now_v7(), "workspace", 2.0);
        let second = hit(Uuid::now_v7(), "workspace", 1.0);

        let reranked = retriever
            .rerank_hits(&req, &[first.clone(), second.clone()])
            .await
            .expect("rerank should succeed");

        assert_eq!(reranked, vec![second]);
    }

    #[tokio::test]
    async fn reranker_receives_hydrated_chunk_text_for_knowledge_hits() {
        // Pins: provider rerankers see the full hydrated knowledge chunk rather
        // than the graph sidecar name, which is too thin for document reranking.
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let retriever = HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
        .with_reranker(Arc::new(RecordingReranker {
            documents: Arc::clone(&observed),
        }));
        let req = RetrievalRequest {
            seeds: Vec::new(),
            query_text: "how do I connect a custom domain?".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: 1,
            use_reranker: true,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        };
        let mut candidate = hit(Uuid::now_v7(), "tenant", 1.0);
        candidate.node.name = "thin sidecar name".to_string();
        candidate.knowledge_chunk = Some(knowledge_chunk(
            "Custom domains connect through DNS records in your site dashboard.",
        ));

        retriever
            .rerank_hits(&req, &[candidate])
            .await
            .expect("rerank should receive hydrated documents");

        let documents = observed.lock().expect("observed rerank documents");
        assert_eq!(documents.len(), 1);
        assert!(
            documents[0].contains("Custom domains connect through DNS records"),
            "reranker document should include chunk text: {}",
            documents[0]
        );
        assert!(
            !documents[0].contains("thin sidecar name"),
            "reranker document should not fall back to sidecar name when chunk text exists"
        );
    }

    #[test]
    fn feature_ranker_rescues_lexical_non_vector_hit_over_vector_noise() {
        // Pins: deterministic ranking can promote exact lexical hits that vector retrieval missed.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let lexical_uid = Uuid::now_v7();
        let vector_uid = Uuid::now_v7();
        let mut lexical_hit = hit(lexical_uid, "workspace", 0.8);
        lexical_hit.legs = LegSources {
            graph: false,
            vector: false,
            lexical: true,
        };
        lexical_hit.node.name = "contact email".to_string();
        lexical_hit.node.valid_from = reference_time;
        lexical_hit.node.last_accessed_at = reference_time;
        lexical_hit.node.properties_summary = Some(serde_json::json!({
            "predicate": "contact_email",
            "object": "user@example.invalid"
        }));
        let mut vector_hit = hit(vector_uid, "workspace", 1.0);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.name = "private repository".to_string();
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut hits = vec![vector_hit, lexical_hit];

        rank_hydrated_hits(
            &mut hits,
            &RankingConfig::default(),
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "contact email".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, lexical_uid);
    }

    #[test]
    fn feature_ranker_rescue_skips_graph_lexical_neighbors() {
        // Pins: lexical rescue is for lexical-only hits, not graph-expanded neighbors.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let lexical_uid = Uuid::now_v7();
        let graph_lexical_uid = Uuid::now_v7();
        let mut lexical_hit = hit(lexical_uid, "workspace", 1.0);
        lexical_hit.legs = LegSources {
            graph: false,
            vector: false,
            lexical: true,
        };
        lexical_hit.node.valid_from = reference_time;
        lexical_hit.node.last_accessed_at = reference_time;
        let mut graph_lexical_hit = hit(graph_lexical_uid, "workspace", 1.0);
        graph_lexical_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: true,
        };
        graph_lexical_hit.node.valid_from = reference_time;
        graph_lexical_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![graph_lexical_hit, lexical_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "regional network".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, lexical_uid);
    }

    #[test]
    fn feature_ranker_rescues_graph_only_expansion_hit() {
        // Pins: deterministic ranking can promote graph-only expansion hits that vector and lexical missed.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let graph_uid = Uuid::now_v7();
        let vector_uid = Uuid::now_v7();
        let mut graph_hit = hit(graph_uid, "workspace", 0.8);
        graph_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: false,
        };
        graph_hit.node.valid_from = reference_time;
        graph_hit.node.last_accessed_at = reference_time;
        let mut vector_hit = hit(vector_uid, "workspace", 1.0);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![vector_hit, graph_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "library owner".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, graph_uid);
    }

    #[test]
    fn anchored_rescue_preserves_vector_rank_one_over_graph_only_hit() {
        // Pins: AnchoredRescue graph-only evidence is not enough to demote the
        // vector winner; later graph policies must add an explicit threshold.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let graph_uid = Uuid::from_u128(10);
        let vector_uid = Uuid::from_u128(20);
        let mut graph_hit = hit(graph_uid, "tenant", 1.0);
        graph_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: false,
        };
        graph_hit.node.valid_from = reference_time;
        graph_hit.node.last_accessed_at = reference_time;
        let mut vector_hit = hit(vector_uid, "tenant", 0.1);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.overlap = 0.0;
        config.weights.quality = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![graph_hit, vector_hit];

        rank_hydrated_hits_for_policy(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "library owner".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            GraphRetrievalPolicy::AnchoredRescue,
            Some(vector_uid),
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(hits[1].uid, graph_uid);
    }

    #[test]
    fn source_graph_ranking_groups_chunks_and_reports_typed_graph_features() {
        // Pins: SourceGraph ranks tenant knowledge at source object and reports
        // typed graph evidence without using noisy same-source-object coherence
        // bonuses.
        let vector_uid = Uuid::from_u128(1);
        let graph_uid = Uuid::from_u128(2);
        let support_uid = Uuid::from_u128(3);
        let vector_article = Uuid::from_u128(10);
        let graph_article = Uuid::from_u128(20);
        let mut vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Generic site settings",
            0,
            1.00,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let mut graph_hit = tenant_chunk_hit(
            graph_uid,
            graph_article,
            "Custom domain DNS records",
            4,
            0.98,
            LegSources {
                graph: true,
                vector: true,
                lexical: true,
            },
        );
        graph_hit
            .knowledge_chunk
            .as_mut()
            .expect("chunk")
            .heading_path = vec!["Custom domain DNS records".to_string()];
        let support_hit = tenant_chunk_hit(
            support_uid,
            graph_article,
            "Custom domain DNS records",
            5,
            0.75,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        vector_hit
            .knowledge_chunk
            .as_mut()
            .expect("chunk")
            .heading_path = vec!["Generic site settings".to_string()];
        let mut hits = vec![vector_hit, graph_hit, support_hit];
        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 3,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[GraphPathTrace {
                seed_uid: Uuid::from_u128(99),
                seed_source: Some(GraphSeedSource::ExactPhaseOne),
                candidate_uid: graph_uid,
                hop: 1,
                edge_labels: vec!["MENTIONED_IN".to_string()],
                edge_directions: vec!["incoming".to_string()],
            }],
            Some(vector_uid),
            GraphRetrievalPolicy::SourceGraph,
        );

        assert_eq!(hits[0].uid, graph_uid);
        assert_eq!(hits[1].uid, support_uid);
        assert_eq!(hits[2].uid, vector_uid);
        assert!(diagnostics.enabled);
        assert_eq!(diagnostics.ranked_source_object_count, 2);
        assert_eq!(diagnostics.top_source_objects[0].object_uid, graph_article);
        assert_eq!(
            diagnostics.top_source_objects[0].rank_before_source_graph,
            Some(2)
        );
        assert_eq!(diagnostics.top_source_objects[0].rank_after_source_graph, 1);
        assert_eq!(
            diagnostics.top_source_objects[0].rank_delta_after_minus_before,
            Some(-1)
        );
        assert_eq!(
            diagnostics.top_source_objects[0].typed_graph_evidence_count,
            1
        );
        assert!(diagnostics.feature_totals.typed_graph_evidence > 0.0);
        assert_eq!(diagnostics.feature_totals.same_source_object_repeat, 0.0);
        assert_eq!(diagnostics.feature_totals.adjacent_chunk_support, 0.0);
    }

    #[test]
    fn source_graph_preserves_vector_article_without_typed_graph_evidence() {
        // Pins: SourceGraph title signals are context organization evidence,
        // not enough by themselves to demote the vector rank-1 article.
        let vector_uid = Uuid::from_u128(11);
        let repeated_uid = Uuid::from_u128(12);
        let vector_article = Uuid::from_u128(110);
        let repeated_article = Uuid::from_u128(120);
        let vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Vector winner",
            0,
            1.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let repeated_hit = tenant_chunk_hit(
            repeated_uid,
            repeated_article,
            "Custom domain DNS records",
            0,
            2.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![vector_hit, repeated_hit];

        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[],
            Some(vector_uid),
            GraphRetrievalPolicy::SourceGraph,
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(hits[1].uid, repeated_uid);
        assert_eq!(diagnostics.top_source_objects[0].object_uid, vector_article);
    }

    #[test]
    fn source_graph_keeps_original_order_when_top_article_is_unchanged() {
        // Pins: SourceGraph should not reshuffle lower-ranked articles when
        // article scoring does not change the top article.
        let top_uid = Uuid::from_u128(21);
        let second_uid = Uuid::from_u128(22);
        let boosted_uid = Uuid::from_u128(23);
        let top_article = Uuid::from_u128(210);
        let second_article = Uuid::from_u128(220);
        let boosted_article = Uuid::from_u128(230);
        let top_hit = tenant_chunk_hit(
            top_uid,
            top_article,
            "Vector winner",
            0,
            2.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let second_hit = tenant_chunk_hit(
            second_uid,
            second_article,
            "Generic settings",
            0,
            0.95,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let boosted_hit = tenant_chunk_hit(
            boosted_uid,
            boosted_article,
            "Custom domain DNS records",
            0,
            0.94,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![top_hit, second_hit, boosted_hit];

        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 3,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[],
            Some(top_uid),
            GraphRetrievalPolicy::SourceGraph,
        );

        assert_eq!(
            hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![top_uid, second_uid, boosted_uid]
        );
        assert_eq!(diagnostics.top_source_objects[0].object_uid, top_article);
        assert_eq!(diagnostics.top_source_objects[1].object_uid, second_article);
        assert_eq!(
            diagnostics.top_source_objects[2].object_uid,
            boosted_article
        );
        assert!(
            diagnostics.top_source_objects[2].features.lexical_title
                > diagnostics.top_source_objects[1].features.lexical_title
        );
    }

    #[test]
    fn entity_local_search_keeps_original_order_when_top_article_is_unchanged() {
        // Pins: EntityLocalSearch uses semantic graph evidence conservatively;
        // it should not reshuffle lower-ranked articles when the top article
        // stays unchanged.
        let top_uid = Uuid::from_u128(26);
        let second_uid = Uuid::from_u128(27);
        let boosted_uid = Uuid::from_u128(28);
        let top_article = Uuid::from_u128(260);
        let second_article = Uuid::from_u128(270);
        let boosted_article = Uuid::from_u128(280);
        let top_hit = tenant_chunk_hit(
            top_uid,
            top_article,
            "Vector winner",
            0,
            2.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let second_hit = tenant_chunk_hit(
            second_uid,
            second_article,
            "Generic settings",
            0,
            0.95,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let boosted_hit = tenant_chunk_hit(
            boosted_uid,
            boosted_article,
            "Custom domain DNS records",
            0,
            0.94,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![top_hit, second_hit, boosted_hit];

        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 3,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[GraphPathTrace {
                seed_uid: Uuid::from_u128(99),
                seed_source: Some(GraphSeedSource::SemanticEntity),
                candidate_uid: boosted_uid,
                hop: 1,
                edge_labels: vec!["MENTIONED_IN".to_string()],
                edge_directions: vec!["incoming".to_string()],
            }],
            Some(top_uid),
            GraphRetrievalPolicy::EntityLocalSearch,
        );

        assert_eq!(
            hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![top_uid, second_uid, boosted_uid]
        );
        assert_eq!(diagnostics.top_source_objects[0].object_uid, top_article);
        assert_eq!(diagnostics.top_source_objects[1].object_uid, second_article);
        assert_eq!(
            diagnostics.top_source_objects[2].object_uid,
            boosted_article
        );
        assert!(
            diagnostics.top_source_objects[2]
                .features
                .typed_graph_evidence
                > 0.0,
            "the semantic graph signal should be present but gated from lower-rank reshuffling"
        );
    }

    #[test]
    fn entity_local_source_object_ranking_preserves_vector_rank_one_with_semantic_path() {
        // Pins: exact entity-local graph evidence is an source-object feature, not
        // enough by itself to demote the vector rank-one article.
        let vector_uid = Uuid::from_u128(31);
        let graph_uid = Uuid::from_u128(32);
        let vector_article = Uuid::from_u128(310);
        let graph_article = Uuid::from_u128(320);
        let vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Vector winner",
            0,
            1.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let graph_hit = tenant_chunk_hit(
            graph_uid,
            graph_article,
            "Custom domain DNS records",
            0,
            0.98,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![vector_hit, graph_hit];

        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[GraphPathTrace {
                seed_uid: Uuid::from_u128(99),
                seed_source: Some(GraphSeedSource::SemanticEntity),
                candidate_uid: graph_uid,
                hop: 1,
                edge_labels: vec!["MENTIONED_IN".to_string()],
                edge_directions: vec!["incoming".to_string()],
            }],
            Some(vector_uid),
            GraphRetrievalPolicy::EntityLocalSearch,
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(hits[1].uid, graph_uid);
        assert_eq!(diagnostics.top_source_objects[0].object_uid, vector_article);
        assert!(diagnostics.feature_totals.typed_graph_evidence > 0.0);
    }

    #[test]
    fn entity_local_source_object_ranking_ignores_disallowed_raw_paths() {
        // Pins: entity-local source-object evidence counts only precise entity-to-
        // chunk paths, not every raw graph traversal returned for diagnostics.
        let vector_uid = Uuid::from_u128(41);
        let graph_uid = Uuid::from_u128(42);
        let vector_article = Uuid::from_u128(410);
        let graph_article = Uuid::from_u128(420);
        let vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Vector winner",
            0,
            1.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let graph_hit = tenant_chunk_hit(
            graph_uid,
            graph_article,
            "Custom domain DNS records",
            0,
            0.98,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![vector_hit, graph_hit];

        let diagnostics = apply_source_object_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[GraphPathTrace {
                seed_uid: Uuid::from_u128(99),
                seed_source: Some(GraphSeedSource::SemanticEntity),
                candidate_uid: graph_uid,
                hop: 2,
                edge_labels: vec!["CONTAINS".to_string(), "CONTAINS".to_string()],
                edge_directions: vec!["incoming".to_string(), "outgoing".to_string()],
            }],
            Some(vector_uid),
            GraphRetrievalPolicy::EntityLocalSearch,
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(diagnostics.feature_totals.typed_graph_evidence, 0.0);
        assert_eq!(diagnostics.feature_totals.structural_only_penalty, 0.0);
    }

    #[test]
    fn source_graph_selection_prioritizes_unique_articles_before_support_chunks() {
        // Pins: SourceGraph final context covers more articles before adding a
        // second chunk from an already-selected article.
        let article_a = Uuid::from_u128(10);
        let article_b = Uuid::from_u128(20);
        let article_c = Uuid::from_u128(30);
        let a1_uid = Uuid::from_u128(101);
        let a2_uid = Uuid::from_u128(102);
        let b1_uid = Uuid::from_u128(201);
        let c1_uid = Uuid::from_u128(301);
        let hits = vec![
            tenant_chunk_hit(
                a1_uid,
                article_a,
                "Article A",
                0,
                1.0,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                a2_uid,
                article_a,
                "Article A",
                1,
                0.9,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                b1_uid,
                article_b,
                "Article B",
                0,
                0.8,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                c1_uid,
                article_c,
                "Article C",
                0,
                0.7,
                LegSources::default(),
            ),
        ];

        let selected =
            select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::SourceGraph);

        assert_eq!(
            selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![a1_uid, b1_uid, c1_uid]
        );
    }

    #[test]
    fn source_graph_selection_adds_support_chunks_after_article_diversity() {
        // Pins: SourceGraph still includes same-source-object support when the final
        // context has room after unique source objects are represented.
        let article_a = Uuid::from_u128(10);
        let article_b = Uuid::from_u128(20);
        let a1_uid = Uuid::from_u128(101);
        let b1_uid = Uuid::from_u128(201);
        let a2_uid = Uuid::from_u128(102);
        let hits = vec![
            tenant_chunk_hit(
                a1_uid,
                article_a,
                "Article A",
                0,
                1.0,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                b1_uid,
                article_b,
                "Article B",
                0,
                0.9,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                a2_uid,
                article_a,
                "Article A",
                1,
                0.8,
                LegSources::default(),
            ),
        ];

        let selected =
            select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::SourceGraph);

        assert_eq!(
            selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![a1_uid, b1_uid, a2_uid]
        );
    }

    #[test]
    fn non_source_graph_selection_keeps_existing_support_order() {
        // Pins: source-diverse selection does not change AnchoredRescue or
        // legacy final-hit ordering.
        let article_a = Uuid::from_u128(10);
        let article_b = Uuid::from_u128(20);
        let a1_uid = Uuid::from_u128(101);
        let a2_uid = Uuid::from_u128(102);
        let b1_uid = Uuid::from_u128(201);
        let hits = vec![
            tenant_chunk_hit(
                a1_uid,
                article_a,
                "Article A",
                0,
                1.0,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                a2_uid,
                article_a,
                "Article A",
                1,
                0.9,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                b1_uid,
                article_b,
                "Article B",
                0,
                0.8,
                LegSources::default(),
            ),
        ];

        let selected =
            select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::AnchoredRescue);

        assert_eq!(
            selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![a1_uid, a2_uid, b1_uid]
        );
    }

    #[test]
    fn entity_local_search_uses_source_diverse_context_selection() {
        // Pins: entity-local semantic graph evidence reuses the SourceGraph
        // source-object context path instead of falling back to chunk order.
        let article_a = Uuid::from_u128(10);
        let article_b = Uuid::from_u128(20);
        let a1_uid = Uuid::from_u128(101);
        let a2_uid = Uuid::from_u128(102);
        let b1_uid = Uuid::from_u128(201);
        let hits = vec![
            tenant_chunk_hit(
                a1_uid,
                article_a,
                "Article A",
                0,
                1.0,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                a2_uid,
                article_a,
                "Article A",
                1,
                0.9,
                LegSources::default(),
            ),
            tenant_chunk_hit(
                b1_uid,
                article_b,
                "Article B",
                0,
                0.8,
                LegSources::default(),
            ),
        ];

        let selected =
            select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::EntityLocalSearch);

        assert_eq!(
            selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            vec![a1_uid, b1_uid, a2_uid]
        );
        assert!(GraphRetrievalPolicy::EntityLocalSearch.uses_source_object_ranking());
        assert!(!GraphRetrievalPolicy::EntityLocalSearch.uses_graph_candidate_fusion());
    }

    #[test]
    fn graph_candidate_counts_split_graph_overlap_buckets() {
        // Pins: report diagnostics distinguish graph-only, pairwise overlap,
        // and all-leg candidates instead of reporting one aggregate graph count.
        let fused = vec![
            (
                Uuid::from_u128(1),
                1.0,
                LegSources {
                    graph: true,
                    vector: false,
                    lexical: false,
                },
            ),
            (
                Uuid::from_u128(2),
                1.0,
                LegSources {
                    graph: true,
                    vector: true,
                    lexical: false,
                },
            ),
            (
                Uuid::from_u128(3),
                1.0,
                LegSources {
                    graph: true,
                    vector: false,
                    lexical: true,
                },
            ),
            (
                Uuid::from_u128(4),
                1.0,
                LegSources {
                    graph: true,
                    vector: true,
                    lexical: true,
                },
            ),
            (
                Uuid::from_u128(5),
                1.0,
                LegSources {
                    graph: false,
                    vector: true,
                    lexical: true,
                },
            ),
        ];

        assert_eq!(
            graph_candidate_counts(&fused),
            GraphCandidateCounts {
                graph_only: 1,
                vector_graph: 1,
                lexical_graph: 1,
                all_legs: 1,
            }
        );
    }

    #[test]
    fn strategy_weighting_unchanged_after_two_phase_restructure() {
        // Pins: GraphFirst still halves only the lexical fusion weight.
        assert_eq!(
            weights_for(Strategy::GraphFirst),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT * 0.5)
        );
        assert_eq!(
            weights_for(Strategy::VectorFirst),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT)
        );
        assert_eq!(
            weights_for(Strategy::Both),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT)
        );
    }

    #[test]
    fn turbopuffer_bm25_chunk_leg_is_boost_only() {
        // Pins: tenant knowledge BM25 may boost vector candidates, but it cannot
        // flood final chunk retrieval with BM25-only candidates.
        let shared = Uuid::from_u128(1);
        let lexical_only = Uuid::from_u128(2);
        let vector_hits = vec![leg_candidate(shared)];
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only), leg_candidate(shared)],
            LexicalBackend::TurbopufferBm25,
        );
        let mut req = vector_request();
        req.label_filter = Some(vec![NodeLabel::Chunk]);

        apply_lexical_boost_only_policy(&req, &vector_hits, &mut lexical_hits);

        assert_eq!(lexical_hits.candidates.len(), 1);
        assert_eq!(lexical_hits.candidates[0].uid, shared);
        assert_eq!(
            lexical_hits.candidates[0].score,
            leg_candidate(shared).score * TURBOPUFFER_BM25_BOOST_MULTIPLIER
        );
    }

    #[test]
    fn turbopuffer_bm25_lexical_only_request_keeps_candidates() {
        // Pins: exact lexical tenant-chunk requests without a query embedding do
        // not drop all BM25 candidates just because the vector leg is empty.
        let lexical_only = Uuid::from_u128(2);
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only)],
            LexicalBackend::TurbopufferBm25,
        );
        let mut req = vector_request();
        req.query_embedding.clear();
        req.label_filter = Some(vec![NodeLabel::Chunk]);

        apply_lexical_boost_only_policy(&req, &[], &mut lexical_hits);

        assert_eq!(lexical_hits.candidates, vec![leg_candidate(lexical_only)]);
    }

    #[test]
    fn postgres_lexical_leg_can_still_add_candidates() {
        // Pins: the boost-only rule is scoped to Turbopuffer BM25 tenant chunks;
        // existing Postgres lexical behavior for memory/exact matches is unchanged.
        let lexical_only = Uuid::from_u128(2);
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only)],
            LexicalBackend::PostgresTsvector,
        );
        let req = vector_request();

        apply_lexical_boost_only_policy(&req, &[], &mut lexical_hits);

        assert_eq!(lexical_hits.candidates, vec![leg_candidate(lexical_only)]);
    }

    #[test]
    fn vector_first_strategy_disables_turbopuffer_bm25() {
        // Pins: tenant-KB vector-first retrieval does not pay for, or fuse in,
        // the Turbopuffer BM25 leg after WixQA showed it hurts rank quality.
        let mut req = vector_request();
        req.label_filter = Some(vec![NodeLabel::Chunk]);
        req.strategy = Some(Strategy::VectorFirst);

        assert!(!request_allows_tenant_chunk_bm25(&req));
    }

    #[test]
    fn fused_candidate_limit_scales_with_requested_final_count() {
        // Pins: widened retrieval cutoffs are not silently capped at the old
        // fixed candidate pool, but production requests still have a hard cap.
        assert_eq!(fused_candidate_limit(0), 0);
        assert_eq!(fused_candidate_limit(5), MIN_FUSED_CANDIDATE_LIMIT);
        assert_eq!(fused_candidate_limit(25), 50);
        assert_eq!(fused_candidate_limit(50), 100);
        assert_eq!(fused_candidate_limit(500), 100);
    }

    #[test]
    fn empty_fusion_retries_vector_when_timeout_can_mask_candidates() {
        // Pins: a timed-out vector leg can otherwise make retrieval return no
        // candidates for an embedded query; one uncapped vector retry is allowed.
        let req = vector_request();

        assert!(should_retry_vector_after_empty_fusion(&req, &[], &[], &[]));
    }

    #[test]
    fn empty_fusion_vector_retry_stays_off_when_any_leg_has_candidates() {
        // Pins: the retry is only for complete candidate loss, not for normal
        // low-recall or graph/lexical-only retrieval states.
        let req = vector_request();
        let candidate = leg_candidate(Uuid::from_u128(1));

        assert!(!should_retry_vector_after_empty_fusion(
            &req,
            &[candidate],
            &[],
            &[]
        ));
        assert!(!should_retry_vector_after_empty_fusion(
            &req,
            &[],
            &[candidate],
            &[]
        ));
        assert!(!should_retry_vector_after_empty_fusion(
            &req,
            &[],
            &[],
            &[candidate]
        ));
    }

    #[test]
    fn empty_fusion_vector_retry_respects_timeout_override() {
        // Pins: callers that disabled leg timeouts already asked for uncapped
        // leg execution, so the empty-fusion retry would only duplicate work.
        let mut req = vector_request();
        req.disable_leg_timeouts = true;

        assert!(!should_retry_vector_after_empty_fusion(&req, &[], &[], &[]));
    }

    #[tokio::test]
    async fn retrieve_returns_empty_when_k_final_is_zero() {
        // Pins: a zero-budget retrieval short-circuits before touching any leg.
        let retriever = lazy_retriever();
        let hits = retriever
            .retrieve(empty_corpus_request(0, false))
            .await
            .expect("k_final=0 should early-return");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn retrieve_returns_empty_for_empty_corpus() {
        // Pins: when every leg yields nothing the fused set is empty and retrieval
        // returns [] without hydrating nodes. Hermetic because an empty query and
        // empty embedding keep all three legs off the database.
        let retriever = lazy_retriever();
        let hits = retriever
            .retrieve(empty_corpus_request(5, false))
            .await
            .expect("empty corpus should return []");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn retrieve_does_not_invoke_reranker_for_empty_corpus() {
        // Pins: the billed reranker is not called when no candidates exceed
        // k_final (here the corpus is empty), even with use_reranker = true.
        let retriever = lazy_retriever().with_reranker(Arc::new(PanicReranker));
        let hits = retriever
            .retrieve(empty_corpus_request(5, true))
            .await
            .expect("empty corpus should return [] without reranking");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn turbopuffer_vector_leg_requires_configured_client() {
        // Pins: a Turbopuffer-selected cloud partition fails closed when the
        // client is missing instead of silently using pgvector.
        let retriever = lazy_retriever();
        let error = retriever
            .vector_leg(
                &vector_request(),
                &VectorBackendState {
                    vector_backend: "turbopuffer".to_string(),
                    vector_backend_state: "steady".to_string(),
                    dual_read_until: None,
                },
            )
            .await
            .expect_err("Turbopuffer backend selection should require a client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn turbopuffer_lexical_leg_requires_configured_client_for_chunk_bm25() {
        // Pins: tenant knowledge chunk BM25 is a Turbopuffer cloud path and must
        // not degrade to Postgres lexical when the client is absent.
        let retriever = lazy_retriever();
        let mut req = vector_request();
        req.query_embedding.clear();
        req.query_text = "deployment runbook".to_string();
        req.label_filter = Some(vec![NodeLabel::Chunk]);
        let error = retriever
            .lexical_leg(
                &req,
                &VectorBackendState {
                    vector_backend: "turbopuffer".to_string(),
                    vector_backend_state: "steady".to_string(),
                    dual_read_until: None,
                },
            )
            .await
            .expect_err("Turbopuffer BM25 backend selection should require a client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn turbopuffer_dual_read_requires_configured_client() {
        // Pins: promotion dual-read is part of the cloud Turbopuffer path and
        // should fail clearly if the client is missing.
        let retriever = lazy_retriever();
        let error = retriever
            .dual_read_vector_leg(&vector_request())
            .await
            .expect_err("dual-read should require a Turbopuffer client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[test]
    fn leg_overlap_measures_top_k_set_intersection() {
        // Pins: dual-read overlap is the top-k uid intersection size over the
        // larger top-k set, and it honors the k cutoff.
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let d = Uuid::from_u128(4);

        // Disjoint top-k -> zero overlap.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(c), leg_candidate(d)],
                10,
            ),
            0.0
        );
        // Identical top-k -> full overlap.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(a), leg_candidate(b)],
                10,
            ),
            1.0
        );
        // Partial overlap: {a,b} vs {a,c} over k=2 -> 1/2.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(a), leg_candidate(c)],
                2,
            ),
            0.5
        );
        // k cutoff: the lists diverge only past position 2, so the top-2 overlap
        // is full even though the full lists differ.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b), leg_candidate(c)],
                &[leg_candidate(b), leg_candidate(a), leg_candidate(d)],
                2,
            ),
            1.0
        );
    }

    #[test]
    fn is_dual_read_active_respects_state_and_expiry() {
        // Pins: dual-read is active only in the dual_read state and only while the
        // dual_read_until deadline is in the future (or unset).
        let future = Utc::now() + chrono::Duration::hours(1);
        let past = Utc::now() - chrono::Duration::hours(1);

        assert!(
            VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: Some(future),
            }
            .is_dual_read_active(),
            "dual_read with a future deadline is active"
        );
        assert!(
            VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: None,
            }
            .is_dual_read_active(),
            "dual_read with no deadline is active"
        );
        assert!(
            !VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: Some(past),
            }
            .is_dual_read_active(),
            "an expired deadline ends dual-read"
        );
        assert!(
            !VectorBackendState {
                vector_backend: "pgvector".to_string(),
                vector_backend_state: "steady".to_string(),
                dual_read_until: Some(future),
            }
            .is_dual_read_active(),
            "the steady state is never dual-read"
        );
    }

    fn lazy_retriever() -> HybridRetriever {
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
    }

    fn empty_corpus_request(k_final: usize, use_reranker: bool) -> RetrievalRequest {
        RetrievalRequest {
            seeds: Vec::new(),
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final,
            use_reranker,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(Utc::now()),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        }
    }

    fn vector_request() -> RetrievalRequest {
        RetrievalRequest {
            query_text: "deployment runbook".to_string(),
            query_embedding: vec![0.0; 1024],
            ..empty_corpus_request(5, false)
        }
    }

    fn leg_candidate(uid: Uuid) -> LegCandidate {
        LegCandidate { uid, score: 1.0 }
    }

    struct PanicReranker;

    #[async_trait]
    impl Reranker for PanicReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            _documents: &[String],
            _top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            panic!("reranker must not be called when no candidates exceed k_final");
        }
    }

    struct RecordingReranker {
        documents: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Reranker for RecordingReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            documents: &[String],
            top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            *self
                .documents
                .lock()
                .expect("recording reranker document lock") = documents.to_vec();
            Ok((0..documents.len().min(top_n))
                .map(|index| RerankHit {
                    index,
                    relevance_score: 1.0,
                })
                .collect())
        }
    }

    fn hit(uid: Uuid, scope: &str, score: f64) -> RetrievalHit {
        RetrievalHit {
            uid,
            score,
            legs: LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
            lexical_backend: None,
            source_tier: SourceTier::UserMemory,
            knowledge_chunk: None,
            node: NodeIndexRow {
                uid,
                label: NodeLabel::Fact,
                storage_partition_id: Some("tenant".to_string()),
                contact_id: None,
                scope: scope.to_string(),
                name: format!("{scope} fact"),
                pii_class: PiiClass::None,
                valid_to: None,
                valid_from: Utc::now(),
                properties_summary: None,
                last_accessed_at: Utc::now(),
                quality_score: 0.5,
            },
        }
    }

    fn knowledge_chunk(text: &str) -> KnowledgeChunkHydration {
        KnowledgeChunkHydration {
            chunk_uid: Uuid::now_v7(),
            document_version_uid: Uuid::now_v7(),
            object_uid: Uuid::now_v7(),
            chunk_hash: "chunk-hash".to_string(),
            ordinal: 0,
            heading_path: vec!["Domains".to_string()],
            text: text.to_string(),
            token_count: 16,
            metadata: Value::Null,
            source_uri: Some("https://support.example.invalid/domain".to_string()),
            source_title: Some("Custom domains".to_string()),
            object_type: "article".to_string(),
        }
    }

    fn tenant_chunk_hit(
        uid: Uuid,
        object_uid: Uuid,
        source_title: &str,
        ordinal: i32,
        score: f64,
        legs: LegSources,
    ) -> RetrievalHit {
        let mut hit = hit(uid, "tenant", score);
        hit.legs = legs;
        hit.source_tier = SourceTier::TenantKnowledge;
        hit.node.label = NodeLabel::Chunk;
        hit.knowledge_chunk = Some(KnowledgeChunkHydration {
            chunk_uid: Uuid::from_u128(10_000 + uid.as_u128()),
            document_version_uid: Uuid::from_u128(20_000 + object_uid.as_u128()),
            object_uid,
            chunk_hash: format!("chunk-{uid}"),
            ordinal,
            heading_path: vec![source_title.to_string()],
            text: format!("{source_title} body"),
            token_count: 16,
            metadata: Value::Null,
            source_uri: Some(format!("https://support.example.invalid/{object_uid}")),
            source_title: Some(source_title.to_string()),
            object_type: "article".to_string(),
        });
        hit
    }

    struct ReverseReranker;

    #[async_trait]
    impl Reranker for ReverseReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            documents: &[String],
            top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            Ok((0..documents.len())
                .rev()
                .take(top_n)
                .map(|index| RerankHit {
                    index,
                    relevance_score: 1.0,
                })
                .collect())
        }
    }

    struct EmptyGraph;

    #[async_trait]
    impl GraphStore for EmptyGraph {
        async fn create_node(
            &self,
            _intent: moa_memory_graph::NodeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn supersede_node(
            &self,
            _old_uid: Uuid,
            _intent: moa_memory_graph::NodeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn invalidate_node(
            &self,
            _uid: Uuid,
            _reason: &str,
        ) -> std::result::Result<(), GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn hard_purge(
            &self,
            _uid: Uuid,
            _redaction_marker: &str,
        ) -> std::result::Result<(), GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn create_edge(
            &self,
            _intent: moa_memory_graph::EdgeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn get_node(
            &self,
            _uid: Uuid,
        ) -> std::result::Result<Option<NodeIndexRow>, GraphError> {
            Ok(None)
        }

        async fn neighbors(
            &self,
            _seed: Uuid,
            _hops: u8,
            _edge_filter: Option<&[moa_memory_graph::EdgeLabel]>,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<NodeIndexRow>, GraphError> {
            Ok(Vec::new())
        }

        async fn expand_seeds(
            &self,
            _seeds: &[Uuid],
            _max_hops: u8,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<moa_memory_graph::GraphExpansionHit>, GraphError> {
            Ok(Vec::new())
        }

        async fn lookup_seeds(
            &self,
            _name: &str,
            _limit: i64,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<NodeIndexRow>, GraphError> {
            Ok(Vec::new())
        }
    }
}
