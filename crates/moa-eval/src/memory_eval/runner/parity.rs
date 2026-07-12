//! Production-parity probe retrieval through the stage-7 evidence seam.
//!
//! Instead of calling `HybridRetriever` directly, parity mode drives
//! `GraphMemoryRetriever::retrieve_evidence` end-to-end: the deterministic
//! lexical router (`route_query`/`decompose_query`), per-scope admission under
//! `MemoryAdmissionPolicy`, the cross-scope merge with `dedupe_and_rank_hits`,
//! and evidence token-budget packing via `render_memory_context_with_budget` —
//! the exact composition production stage 7 uses for prompt injection.

use super::*;

use moa_brain::pipeline::MemoryEvidenceRequest;
use moa_brain::pipeline::memory::{
    GraphMemoryRetriever, ScopedRetrievalRuntime, ScopedRetrievalRuntimeFactory,
};
use moa_brain::retrieval::CachedHybridRetriever;
use moa_core::types::channel::Channel;
use moa_core::types::contact::{ContactRef, ContactVerificationState};
use moa_core::types::context::WorkingContext;
use moa_core::types::identifiers::{ModelId, SessionId};
use moa_core::types::model::{ModelCapabilities, TokenPricing, ToolCallFormat};
use moa_core::types::session::SessionMeta;

/// Fixed evidence token budget used for parity-mode packing measurement.
///
/// Deliberately tight so `render_memory_context_with_budget` exerts real
/// packing pressure on every probe: with up to
/// [`RETRIEVAL_EVAL_CANDIDATE_K`] ranked hits, knowledge chunks and longer
/// fact summaries must compete for the window instead of all fitting. This is
/// a measurement constant for the eval — production stage 7 budgets
/// dynamically (`token_budget / 5` minus the reminder wrapper) — not
/// production configuration.
pub(super) const PARITY_EVIDENCE_TOKEN_BUDGET: usize = 2_048;

/// Scope-runtime factory that rebuilds the isolated eval store's deterministic
/// retrieval backends for every memory scope stage 7 plans.
///
/// Construction mirrors the direct probe path in `retrieve_probe`: app-role
/// scoped pgvector and graph stores over the eval schema plus a
/// `HybridRetriever` carrying the run's deterministic ranking config and
/// reranker. The retriever is wrapped in the production read-time
/// `CachedHybridRetriever`, so the cache-probe-before-embedding ordering of
/// stage 7 is exercised too.
struct EvalScopedRetrievalRuntimeFactory {
    pool: PgPool,
    ranking_config: RankingConfig,
    reranker: Arc<dyn Reranker>,
    exact_vector_search: bool,
}

#[async_trait]
impl ScopedRetrievalRuntimeFactory for EvalScopedRetrievalRuntimeFactory {
    async fn build_runtime(
        &self,
        scope: &MemoryScope,
        _config: &MoaConfig,
        _pool: &PgPool,
        _assume_app_role: bool,
    ) -> moa_core::error::Result<ScopedRetrievalRuntime> {
        let scope_context = scope.to_rls_context();
        let mut vector_store =
            PgvectorStore::new_for_app_role(self.pool.clone(), scope_context.clone());
        if self.exact_vector_search {
            vector_store = vector_store.with_exact_search(true);
        }
        let vector = Arc::new(vector_store);
        let graph_vector: Arc<dyn VectorStore> = vector.clone();
        let graph: Arc<dyn GraphStore> = Arc::new(
            PostgresGraphStore::scoped_for_app_role(self.pool.clone(), scope_context)
                .with_vector_store(graph_vector),
        );
        let hybrid = Arc::new(
            HybridRetriever::new(self.pool.clone(), graph.clone(), vector)
                .with_ranking_config(self.ranking_config.clone())
                .with_reranker(self.reranker.clone())
                .with_assume_app_role(true),
        );
        let cached = Arc::new(CachedHybridRetriever::new_for_app_role(
            hybrid,
            self.pool.clone(),
        ));
        Ok(ScopedRetrievalRuntime::new(graph, cached))
    }
}

/// Ordered parity hits plus the packed rendered-window length for one probe.
pub(super) struct ParityProbeRetrieval {
    /// Ranked admitted hits returned before evidence rendering.
    pub(super) hits: Vec<RetrievalHit>,
    /// Number of ranked hits that survived evidence token-budget packing.
    pub(super) rendered_candidate_count: usize,
    /// End-to-end evidence retrieval latency observed for this probe.
    pub(super) retrieval_latency_ms: u64,
}

/// Runs probes through `GraphMemoryRetriever::retrieve_evidence` end-to-end.
pub(super) struct ParityProbeRetriever {
    retriever: GraphMemoryRetriever,
    deterministic_replay: bool,
}

impl ParityProbeRetriever {
    /// Builds the shared stage-7 retriever over the isolated eval store.
    pub(super) fn new(
        pool: PgPool,
        embedder: Arc<dyn EmbeddingProvider>,
        reranker: Arc<dyn Reranker>,
        ranking_config: RankingConfig,
        window_policy: EvidenceWindowPolicy,
        deterministic_replay: bool,
    ) -> Self {
        let factory = Arc::new(EvalScopedRetrievalRuntimeFactory {
            pool: pool.clone(),
            ranking_config: ranking_config.clone(),
            reranker,
            exact_vector_search: deterministic_replay,
        });
        // Stage 7 populates each request's `EvidenceWindowPolicy` from the
        // retriever's `MoaConfig` memory ranking knobs, so the lane's window
        // policy must ride in through the same config path the production
        // retriever reads (the direct probe path sets it per request).
        let mut config = MoaConfig::default();
        config.memory.retrieval.ranking.rerank_window = window_policy.rerank_window;
        config
            .memory
            .retrieval
            .ranking
            .abstain_below_window_evidence = window_policy.abstain_below_window_evidence;
        let retriever = GraphMemoryRetriever::new_with_config(config, pool, Some(embedder))
            .with_assume_app_role(true)
            .with_scoped_runtime_factory(factory);
        Self {
            retriever,
            deterministic_replay,
        }
    }

    /// Retrieves one probe through the production evidence seam.
    ///
    /// The ranked-hit depth is widened to [`RETRIEVAL_EVAL_CANDIDATE_K`]
    /// through the request-local eval knob the seam exposes, so candidate
    /// attribution matches the direct path; ordering, admission, and packing
    /// are untouched production behavior.
    pub(super) async fn retrieve(&self, probe: &Probe) -> Result<ParityProbeRetrieval> {
        let started = Instant::now();
        let ctx = parity_working_context(probe);
        let request = MemoryEvidenceRequest::new(&probe.query, PARITY_EVIDENCE_TOKEN_BUDGET)
            .with_ranked_occurrence_depth(RETRIEVAL_EVAL_CANDIDATE_K)
            .map_err(|error| memory_retrieval_error(probe, error))?;
        let response = self
            .retriever
            .retrieve_evidence(&ctx, request)
            .await
            .map_err(|error| memory_retrieval_error(probe, error))?;
        // `source_refs` covers exactly the rendered ranked-hit prefix; stage 7
        // asserts `budgeted.hit_count == source_refs.len()`.
        let rendered_candidate_count = response.source_refs.len();
        let retrieval_latency_ms = if self.deterministic_replay {
            0
        } else {
            duration_ms_u64(started.elapsed())
        };
        Ok(ParityProbeRetrieval {
            hits: response.hits,
            rendered_candidate_count,
            retrieval_latency_ms,
        })
    }
}

/// Builds the stage-7 working context for one probe's contact session.
///
/// Tenant and contact identities mirror the corpus store's ingestion scopes;
/// no `agent_context` is pinned, so admission runs under the platform-default
/// knowledge policy exactly as a default contact session would in production
/// (tenant-knowledge labels plus current-contact memory).
fn parity_working_context(probe: &Probe) -> WorkingContext {
    let tenant_id = tenant_id_from_storage_partition_id(&probe.storage_partition_id);
    let contact_id = contact_id_from_user_id(&probe.user_id);
    let session = SessionMeta {
        id: SessionId::new(),
        tenant_id,
        channel: Channel::Chat,
        model: ModelId::new("memory-eval-parity"),
        contact: Some(ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        ..SessionMeta::default()
    };
    WorkingContext::new(&session, parity_model_capabilities())
}

/// Minimal model capabilities for the parity working context.
///
/// `retrieve_evidence` takes its query and token budget from the request, so
/// only structural capability fields matter here.
fn parity_model_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("memory-eval-parity"),
        context_window: 32_000,
        max_output: 1_024,
        supports_tools: false,
        supports_vision: false,
        supports_prefix_caching: false,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}
