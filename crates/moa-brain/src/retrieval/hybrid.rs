//! Production hybrid graph-memory retriever.
//!
//! `HybridRetriever` coordinates graph, vector, lexical, and reranking legs;
//! tenant-knowledge SQL hydration lives in the private `hydration` module.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::config::MoaConfig;
use moa_db::ScopedConn;
use moa_memory_graph::{GraphError, GraphStore, NodeIndexRow, NodeLabel};
use moa_memory_vector::{Error as VectorError, PgvectorStore, TurbopufferStore};
use moa_providers::{ConfiguredReranker, Reranker, build_reranker_from_config};
use sqlx::PgPool;
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::enrichment::EnrichmentHandle;
use crate::retrieval::graph_seed::{
    GraphSeedPlan, hydrate_graph_seed_rows, interim_graph_seed_plan, semantic_entity_seed_uids,
};
use crate::retrieval::hydration::hydrate_knowledge_chunks;
use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, LegCandidate, RRF_K, VECTOR_BUDGET,
    VECTOR_WEIGHT, graph_expansion_leg_with_diagnostics, hydrate_nodes, lexical_leg, rrf_fuse,
    timed_leg, turbopuffer_bm25_leg, vector_leg as run_vector_leg, walk_scoring,
};
use crate::retrieval::policy::{GraphRetrievalPolicy, effective_graph_policy};
use crate::retrieval::ranking::{
    FeatureRanker, RankingConfig, normalize_tokens, ranking_fingerprint,
};
use crate::retrieval::source_rank::{
    apply_source_object_graph_ranking, select_final_hits_for_policy,
};
use crate::retrieval::types::{
    GraphCandidateCounts, GraphRetrievalDiagnostics, LegSources, LexicalBackend, LineageContext,
    Result, RetrievalError, RetrievalHit, RetrievalLineageHit, RetrievalOutput, RetrievalRequest,
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
    lineage_sample_rate: f64,
    graph_policy: GraphRetrievalPolicy,
    enrichment: Option<EnrichmentHandle>,
}

/// Deterministically decides whether one turn's retrieval writes lineage rows.
///
/// The decision hashes `(session_id, turn_seq)` instead of drawing randomness,
/// so replays and reruns of one turn are stable: at a fixed rate a turn either
/// always records lineage or never does. Beta-smoothed quality scores converge
/// on a sample, so partial rates cap lineage write cost at scale without
/// starving the scoring job.
fn lineage_turn_sampled(lineage: &LineageContext, sample_rate: f64) -> bool {
    if sample_rate >= 1.0 {
        return true;
    }
    if sample_rate <= 0.0 {
        return false;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(lineage.session_id.0.as_bytes());
    hasher.update(&lineage.turn_seq.to_be_bytes());
    let digest = hasher.finalize();
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest.as_bytes()[..8]);
    let bucket = u64::from_be_bytes(prefix) as f64 / u64::MAX as f64;
    bucket < sample_rate
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
            lineage_sample_rate: 1.0,
            graph_policy: GraphRetrievalPolicy::default(),
            enrichment: None,
        }
    }

    /// Attaches the shared bounded enrichment worker used for post-retrieval
    /// `last_accessed_at` bumps and sampled lineage writes. When absent, those
    /// best-effort writes are skipped.
    #[must_use]
    pub(crate) fn with_enrichment(mut self, enrichment: Option<EnrichmentHandle>) -> Self {
        self.enrichment = enrichment;
        self
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
            .with_lineage_sample_rate(config.memory.retrieval.lineage_sample_rate)
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

    /// Overrides the fraction of turns that write lineage rows.
    #[must_use]
    pub fn with_lineage_sample_rate(mut self, sample_rate: f64) -> Self {
        self.lineage_sample_rate = sample_rate;
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
        let (vector_outcome, lexical_outcome) = tokio::join!(vector_future, lexical_future);
        // Reduce each leg to its usable hits: a fatal error aborts, a transient
        // error or timeout degrades to empty while the peer leg's hits are kept.
        let vector_leg = reduce_leg("vector", vector_outcome)?;
        let vector_timed_out = vector_leg.timed_out;
        let mut vector_hits = vector_leg.value;
        let mut lexical_hits = reduce_leg("lexical", lexical_outcome)?.value;
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
            let graph_outcome = run_leg(
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
                    &walk_scoring(&self.ranking_config, req.as_of),
                    self.ranking_config.graph_rescue_evidence_floor,
                ),
            )
            .await;
            let mut output = reduce_leg("graph", graph_outcome)?.value;
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
                vector_timed_out,
                &lexical_hits.candidates,
                &graph_hits,
            )
        {
            // Retry only an observed timeout, and under the same leg budget so a
            // slow backend cannot turn one bounded timeout into an uncapped tail.
            let retry_outcome = run_leg(
                req.disable_leg_timeouts,
                "vector",
                VECTOR_BUDGET,
                self.vector_leg(&req, &backend_state),
            )
            .await;
            vector_hits = reduce_leg("vector", retry_outcome)?.value;
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
        let mut hits = build_hits(fused, nodes, &vector_hits);
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
        let mut final_hits = if req.use_reranker && hits.len() > req.k_final {
            let reranked = self.rerank_hits(&req, &hits).await?;
            // A trustworthy reranker concentrates gold at the top, so the
            // reranked window can be tighter than the caller's k: the tail
            // slots are the noise the precision metrics measure. Without a
            // reranker the tail still carries recall and k is kept.
            let rerank_k = match self.ranking_config.rerank_window {
                0 => req.k_final,
                window => req.k_final.min(window),
            };
            select_final_hits_for_policy(reranked, &hits, rerank_k, graph_policy)
        } else {
            select_final_hits_for_policy(hits, &[], req.k_final, graph_policy)
        };
        apply_injection_evidence_floor(&mut final_hits, &self.ranking_config, &req);
        metrics::histogram!("moa_retrieval_rrf_rerank_seconds")
            .record(fusion_started.elapsed().as_secs_f64());

        // F20: post-retrieval enrichment is best-effort and is routed through the
        // shared bounded worker (coalesced, drop-on-overflow, drained on shutdown)
        // instead of an unbounded detached task per retrieval. When no worker is
        // configured (unit/eval retrievers) the writes are simply skipped.
        if let Some(enrichment) = &self.enrichment {
            if req.ranking_reference_time.is_none() {
                let touched_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
                enrichment.enqueue_access_bump(
                    req.scope.clone(),
                    touched_uids,
                    self.assume_app_role,
                );
            }
            if self.lineage_enabled
                && let Some(lineage) = req.lineage
                && lineage_turn_sampled(&lineage, self.lineage_sample_rate)
            {
                let ranked_hits = final_hits
                    .iter()
                    .map(RetrievalLineageHit::from_hit)
                    .collect::<Vec<_>>();
                enrichment.enqueue_lineage(
                    req.scope.clone(),
                    lineage,
                    ranked_hits,
                    Utc::now(),
                    self.assume_app_role,
                );
            }
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
        let reranked = match self
            .reranker
            .rerank(&self.rerank_model, &req.query_text, &documents, req.k_final)
            .await
        {
            Ok(reranked) => reranked,
            // Reranking is a best-effort refinement over already-fused hits: a
            // provider failure must not abort otherwise-usable results. Fall back
            // to the fused pre-rerank order, mirroring the empty-output fallback.
            Err(error) => {
                record_leg_degraded("rerank", "error");
                tracing::warn!(
                    error = %error,
                    "reranker failed; falling back to fused pre-rerank order"
                );
                return Ok(hits.iter().take(req.k_final).cloned().collect());
            }
        };
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
    vector_timed_out: bool,
    lexical_hits: &[LegCandidate],
    graph_hits: &[LegCandidate],
) -> bool {
    // Retry only when the vector leg specifically timed out: a genuinely empty
    // vector result (or a degraded transient failure) is not masking candidates,
    // so re-running it would only duplicate work.
    !req.disable_leg_timeouts
        && !req.query_embedding.is_empty()
        && vector_timed_out
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
            similarity: None,
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

/// Classified outcome of one retrieval leg.
///
/// Threads the *reason* a leg produced no usable hits — instead of collapsing
/// every non-success into an empty default — so the caller can keep a
/// successful peer leg, decide whether a timeout is worth one bounded retry,
/// and abort only on a fatal (RLS/privacy/scope/invalid-config) error.
enum LegOutcome<T> {
    /// The leg completed; `value` may be empty (a genuine empty result).
    Completed(T),
    /// The leg exceeded its time budget.
    Timeout,
    /// The leg hit a transient optional-backend/provider error; degrade it.
    Transient(RetrievalError),
    /// The leg hit an RLS/privacy/scope/invalid-config error; abort retrieval.
    Fatal(RetrievalError),
}

/// One leg reduced to its usable value plus degradation signals.
struct LegDegradation<T> {
    /// Usable hits (empty when the leg degraded or genuinely returned nothing).
    value: T,
    /// Whether the leg degraded (a timeout or transient error, not a real empty).
    #[allow(dead_code)]
    degraded: bool,
    /// Whether the leg specifically timed out; gates the bounded vector retry.
    timed_out: bool,
}

/// Reduces a [`LegOutcome`] to usable hits.
///
/// A completed leg passes through. A timeout or transient error degrades to an
/// empty result (recording a metric and warning) so the peer leg's hits are
/// kept. A fatal error aborts the whole retrieval.
fn reduce_leg<T: Default>(name: &'static str, outcome: LegOutcome<T>) -> Result<LegDegradation<T>> {
    match outcome {
        LegOutcome::Completed(value) => Ok(LegDegradation {
            value,
            degraded: false,
            timed_out: false,
        }),
        LegOutcome::Timeout => {
            record_leg_degraded(name, "timeout");
            tracing::warn!(
                leg = name,
                "hybrid retrieval leg exceeded budget; degrading to empty and keeping peer legs"
            );
            Ok(LegDegradation {
                value: T::default(),
                degraded: true,
                timed_out: true,
            })
        }
        LegOutcome::Transient(error) => {
            record_leg_degraded(name, "transient");
            tracing::warn!(
                leg = name,
                error = %error,
                "hybrid retrieval leg failed transiently; degrading to empty and keeping peer legs"
            );
            Ok(LegDegradation {
                value: T::default(),
                degraded: true,
                timed_out: false,
            })
        }
        LegOutcome::Fatal(error) => Err(error),
    }
}

fn record_leg_degraded(leg: &'static str, reason: &'static str) {
    metrics::counter!("moa_retrieval_leg_degraded_total", "leg" => leg, "reason" => reason)
        .increment(1);
}

fn classify_leg_result<T>(error: RetrievalError) -> LegOutcome<T> {
    if is_fatal_retrieval_error(&error) {
        LegOutcome::Fatal(error)
    } else {
        LegOutcome::Transient(error)
    }
}

/// Classifies a retrieval error as fatal (abort) or transient (degrade).
///
/// Fatal covers RLS/privacy/scope failures and invalid-configuration/contract
/// errors that will not self-heal and must not be silently degraded. Transient
/// covers ordinary optional backend, provider, network, and query failures that
/// should degrade one leg while its peers are kept.
fn is_fatal_retrieval_error(error: &RetrievalError) -> bool {
    match error {
        // Scope/RLS transaction setup failed: privacy boundary, abort.
        RetrievalError::Scope(_) => true,
        // A direct sidecar query failure in the fusion/hydration path is an
        // ordinary backend error.
        RetrievalError::Sqlx(_) => false,
        RetrievalError::Graph(error) => is_fatal_graph_error(error),
        RetrievalError::Vector(error) => is_fatal_vector_error(error),
    }
}

fn is_fatal_graph_error(error: &GraphError) -> bool {
    match error {
        // RLS/privacy/scope invariants.
        GraphError::RlsDenied | GraphError::MissingScope | GraphError::Scope(_) => true,
        // Invalid data/config invariants that will not self-heal.
        GraphError::MissingEmbeddingMetadata
        | GraphError::Conflict(_)
        | GraphError::BiTemporal(_)
        | GraphError::UnknownNodeLabel(_)
        | GraphError::UnknownEdgeLabel(_)
        | GraphError::UnknownPiiClass(_)
        | GraphError::ChangelogScopeMismatch { .. }
        | GraphError::InvalidChangelogScope => true,
        // Ordinary backend/query failures degrade this leg.
        GraphError::GraphQuery(_)
        | GraphError::Sidecar(_)
        | GraphError::NotFound(_)
        | GraphError::Json(_) => false,
        // Delegate to the vector classification.
        GraphError::Vector(error) => is_fatal_vector_error(error),
    }
}

fn is_fatal_vector_error(error: &VectorError) -> bool {
    match error {
        // Embedding/index shape, privacy, backend-capability, and configuration
        // invariants: retrying or degrading would hide a real misconfiguration.
        VectorError::DimensionMismatch { .. }
        | VectorError::UnknownPiiClass(_)
        | VectorError::EmbedderConfig(_)
        | VectorError::EmbedderMismatch { .. }
        | VectorError::StoragePartitionEmbedderStateMissing { .. }
        | VectorError::EmbedderModelMismatch { .. }
        | VectorError::QueryLimitTooLarge(_)
        | VectorError::StoragePartitionRequired { .. }
        | VectorError::TurbopufferUnavailable { .. }
        | VectorError::TurbopufferBaaRequired { .. }
        | VectorError::TurbopufferConfig(_)
        | VectorError::PromotionValidationFailed { .. }
        | VectorError::InvalidPromotionState { .. }
        | VectorError::TransactionalWritesUnsupported(_)
        | VectorError::InvalidVectorSyncOperation(_)
        | VectorError::UnsupportedVectorBackend { .. }
        | VectorError::UnsupportedQueryFeature { .. } => true,
        // Transient backend/provider/network/response failures degrade the leg.
        VectorError::EmbeddingResponseLength { .. }
        | VectorError::ProviderStatus { .. }
        | VectorError::VectorProviderStatus { .. }
        | VectorError::ReembedInProgress { .. }
        | VectorError::TurbopufferResponse(_)
        | VectorError::Core(_)
        | VectorError::Sqlx(_)
        | VectorError::Reqwest(_)
        | VectorError::SerdeJson(_) => false,
    }
}

async fn run_leg<T, F>(
    disable_timeout: bool,
    name: &'static str,
    budget: std::time::Duration,
    future: F,
) -> LegOutcome<T>
where
    T: Default,
    F: std::future::Future<Output = Result<T>>,
{
    if disable_timeout {
        return match future.await {
            Ok(value) => LegOutcome::Completed(value),
            Err(error) => classify_leg_result(error),
        };
    }
    match timed_leg(name, budget, future).await {
        Ok(Ok(value)) => LegOutcome::Completed(value),
        Ok(Err(error)) => classify_leg_result(error),
        Err(_elapsed) => LegOutcome::Timeout,
    }
}

fn build_hits(
    fused: Vec<(Uuid, f64, LegSources)>,
    nodes: Vec<NodeIndexRow>,
    vector_hits: &[LegCandidate],
) -> Vec<RetrievalHit> {
    let similarity_by_uid = vector_hits
        .iter()
        .filter_map(|candidate| {
            candidate
                .similarity
                .map(|similarity| (candidate.uid, similarity))
        })
        .collect::<HashMap<_, _>>();
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
                similarity: similarity_by_uid.get(&uid).copied(),
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

/// Drops final-window hits with no absolute lexical evidence for the query.
///
/// Fused scores are rank-relative: the nearest-neighbor legs always fill the
/// window, so a query with no supporting memory still injects
/// confident-looking noise (measured before this floor as
/// `abstention_false_positive_rate` 1.0 and `precision_at_4` 0.31). The
/// evidence score is absolute, so a floor on it separates "best of nothing"
/// from "supported". Graph-admitted hits are exempt: their evidence is the
/// anchored path that admitted them, and multi-hop facts legitimately share
/// no tokens with the query.
fn apply_injection_evidence_floor(
    hits: &mut Vec<RetrievalHit>,
    config: &RankingConfig,
    req: &RetrievalRequest,
) {
    if config.min_hit_evidence <= 0.0 && config.abstain_below_window_evidence <= 0.0 {
        return;
    }
    let query_tokens = normalize_tokens(&req.query_text);
    if query_tokens.is_empty() {
        return;
    }
    // A hit's evidence clears on either signal: lexical overlap catches
    // exact-term matches the embedding may miss, and raw vector similarity
    // catches paraphrased evidence with no token overlap (dropping those is
    // what sank the lexical-only floor in the 2026-07-11 sweep).
    let evidence_of = |hit: &RetrievalHit| -> f64 {
        // Mirror the ranking stage: knowledge chunks are scored on their text.
        let lexical_evidence = if let Some(chunk) = &hit.knowledge_chunk {
            let mut node = hit.node.clone();
            node.name.clone_from(&chunk.text);
            FeatureRanker::evidence(&query_tokens, &node)
        } else {
            FeatureRanker::evidence(&query_tokens, &hit.node)
        };
        lexical_evidence.max(hit.similarity.unwrap_or(0.0))
    };

    // Whole-window abstention: when the BEST evidence in the window is below
    // the abstain threshold and nothing is graph-admitted, the query has no
    // supporting memory — return nothing instead of nearest-of-nothing noise.
    // Per-hit floors cannot do this job: gold and near-miss evidence overlap
    // per hit, but window maxima separate answerable queries from
    // unanswerable ones (live calibration 2026-07-11: abstention window
    // maxima ≤ 0.67 while answerable windows reach ≥ 0.68).
    if config.abstain_below_window_evidence > 0.0
        && !hits.is_empty()
        && !hits.iter().any(|hit| hit.legs.graph)
    {
        let window_max = hits.iter().map(&evidence_of).fold(0.0_f64, f64::max);
        if window_max < config.abstain_below_window_evidence {
            metrics::counter!("moa_retrieval_window_abstained_total").increment(1);
            hits.clear();
            return;
        }
    }

    if config.min_hit_evidence <= 0.0 {
        return;
    }
    hits.retain(|hit| {
        if hit.legs.graph {
            return true;
        }
        let admitted = evidence_of(hit) >= config.min_hit_evidence;
        if !admitted {
            metrics::counter!("moa_retrieval_evidence_floor_dropped_total").increment(1);
        }
        admitted
    });
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
    rank_hydrated_hits_for_policy(hits, config, req, GraphRetrievalPolicy::default(), None);
}

fn rank_hydrated_hits_for_policy(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
    graph_policy: GraphRetrievalPolicy,
    vector_rank_one: Option<Uuid>,
) {
    apply_feature_ranking(hits, config, req);
    preserve_vector_rank_one_for_policy(hits, graph_policy, vector_rank_one);
}

fn apply_feature_ranking(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
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
        // Graph-only candidates already cleared the policy's admission gates
        // (anchored seeds, path shape, evidence floor), so the rescue weight
        // lifts them over same-feature noise the other legs never surfaced.
        if hit.legs.graph && !hit.legs.vector && !hit.legs.lexical {
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
mod tests;
