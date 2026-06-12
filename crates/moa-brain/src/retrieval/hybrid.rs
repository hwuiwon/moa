//! Production hybrid graph-memory retriever.
//!
//! This remains one module because `HybridRetriever` owns the graph, vector,
//! and reranker boundary while individual retrieval legs live in `legs`.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, MoaConfig, ScopeContext, ScopedConn, SessionId};
use moa_memory_graph::{GraphError, GraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_vector::{Error as VectorError, TurbopufferStore, VectorStore};
use secrecy::SecretString;
use sqlx::PgPool;
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, LegCandidate, VECTOR_BUDGET,
    VECTOR_WEIGHT, bump_last_accessed, graph_expansion_leg, hydrate_nodes, lexical_leg, rrf_fuse,
    timed_leg, vector_leg as run_vector_leg, write_retrieval_lineage,
};
use crate::retrieval::ranking::{
    FeatureRanker, RankingConfig, RankingMode, normalize_tokens, ranking_fingerprint,
};
use crate::retrieval::reranker::{CohereReranker, NoopReranker, Reranker};

const RERANK_MODEL: &str = "rerank-v4.0-fast";
const FUSED_CANDIDATE_LIMIT: usize = 26;
const PHASE_ONE_GRAPH_SEED_LIMIT: usize = FUSED_CANDIDATE_LIMIT;
const PHASE_ONE_SEED_DECAY: f64 = 0.85;

/// Result type returned by hybrid retrieval.
pub type Result<T> = std::result::Result<T, RetrievalError>;

/// Error returned by hybrid retrieval.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// Graph traversal failed.
    #[error("graph retrieval: {0}")]
    Graph(#[from] GraphError),
    /// Vector KNN failed.
    #[error("vector retrieval: {0}")]
    Vector(#[from] VectorError),
    /// Postgres sidecar access failed.
    #[error("postgres retrieval: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Scoped Postgres connection setup failed.
    #[error("scope setup: {0}")]
    Scope(#[from] moa_core::MoaError),
    /// Reranking failed.
    #[error("rerank: {0}")]
    Rerank(String),
}

/// Retrieval request supplied by the query planner.
#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    /// NER seed node ids for graph traversal.
    pub seeds: Vec<Uuid>,
    /// Query text used by lexical retrieval and reranking.
    pub query_text: String,
    /// Dense query embedding used by vector retrieval.
    pub query_embedding: Vec<f32>,
    /// Request memory scope used for sidecar RLS GUCs.
    pub scope: MemoryScope,
    /// Optional graph node label allowlist.
    pub label_filter: Option<Vec<NodeLabel>>,
    /// Maximum PII class visible to the caller.
    pub max_pii_class: PiiClass,
    /// Number of final candidates to return.
    pub k_final: usize,
    /// Whether to apply Cohere-compatible reranking after RRF.
    pub use_reranker: bool,
    /// Optional planner-selected strategy for leg weighting.
    pub strategy: Option<Strategy>,
    /// Optional application-time filter for bitemporal retrieval.
    pub as_of: Option<DateTime<Utc>>,
    /// Optional deterministic clock for post-hydration ranking features.
    pub ranking_reference_time: Option<DateTime<Utc>>,
    /// Optional turn context for fire-and-forget retrieval lineage capture.
    pub lineage: Option<LineageContext>,
    /// Whether retrieval legs should run without timeout budgets.
    pub disable_leg_timeouts: bool,
    /// Whether graph expansion should be skipped for this request.
    pub disable_graph_expansion: bool,
}

/// Per-turn context needed to record retrieved facts for quality scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineageContext {
    /// Session that issued the retrieval.
    pub session_id: SessionId,
    /// Monotonic turn sequence when known.
    pub turn_seq: i64,
}

/// Retrieval legs that contributed to one fused candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LegSources {
    /// Candidate came from graph traversal.
    pub graph: bool,
    /// Candidate came from vector KNN.
    pub vector: bool,
    /// Candidate came from lexical search.
    pub lexical: bool,
}

/// One hydrated retrieval result.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalHit {
    /// Stable graph node uid.
    pub uid: Uuid,
    /// Retrieval score after the configured ranking stage.
    pub score: f64,
    /// Source legs that contributed to the score.
    pub legs: LegSources,
    /// Hydrated sidecar row.
    pub node: NodeIndexRow,
}

/// Hybrid retriever that fuses graph, vector, and lexical retrieval.
#[derive(Clone)]
pub struct HybridRetriever {
    pool: PgPool,
    graph: Arc<dyn GraphStore>,
    vector: Arc<dyn VectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
    reranker: Arc<dyn Reranker>,
    ranking_config: RankingConfig,
    assume_app_role: bool,
    lineage_enabled: bool,
}

impl HybridRetriever {
    /// Creates a hybrid retriever with deterministic no-op reranking.
    #[must_use]
    pub fn new(pool: PgPool, graph: Arc<dyn GraphStore>, vector: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            graph,
            vector,
            turbopuffer: None,
            reranker: Arc::new(NoopReranker),
            ranking_config: RankingConfig::default(),
            assume_app_role: false,
            lineage_enabled: false,
        }
    }

    /// Creates a hybrid retriever using Cohere Rerank when an API key is present.
    #[must_use]
    pub fn from_env(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
    ) -> Self {
        let reranker = std::env::var("COHERE_API_KEY")
            .map(|api_key| {
                Arc::new(CohereReranker::new(SecretString::from(api_key))) as Arc<dyn Reranker>
            })
            .unwrap_or_else(|_| Arc::new(NoopReranker));
        let turbopuffer = TurbopufferStore::from_env().ok().map(Arc::new);
        Self::new(pool, graph, vector)
            .with_turbopuffer(turbopuffer)
            .with_reranker(reranker)
    }

    /// Creates a hybrid retriever from shared config and secret-bearing environment variables.
    #[must_use]
    pub fn from_config(
        config: &MoaConfig,
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
    ) -> Self {
        let reranker = std::env::var(&config.memory.vector.embedder.cohere.api_key_env)
            .map(|api_key| {
                Arc::new(CohereReranker::new(SecretString::from(api_key))) as Arc<dyn Reranker>
            })
            .unwrap_or_else(|_| Arc::new(NoopReranker));
        let turbopuffer = TurbopufferStore::from_config(config).ok().map(Arc::new);
        Self::new(pool, graph, vector)
            .with_turbopuffer(turbopuffer)
            .with_reranker(reranker)
            .with_ranking_config(RankingConfig::from(&config.memory.retrieval.ranking))
            .with_lineage_enabled(config.memory.retrieval.lineage_enabled)
    }

    /// Adds an optional Turbopuffer target backend for promoted workspaces.
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

    /// Overrides the deterministic post-hydration ranking configuration.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
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
        ranking_fingerprint(&self.ranking_config)
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
        if req.k_final == 0 {
            return Ok(Vec::new());
        }

        let strategy = req.strategy.unwrap_or(Strategy::Both);
        let vector_future = run_leg(
            req.disable_leg_timeouts,
            "vector",
            VECTOR_BUDGET,
            self.vector_leg(&req),
        );
        let lexical_future = run_leg(
            req.disable_leg_timeouts,
            "lexical",
            LEXICAL_BUDGET,
            lexical_leg(&self.pool, &req, self.assume_app_role),
        );
        let (vector_hits, lexical_hits) = tokio::join!(vector_future, lexical_future);
        let vector_hits = vector_hits?;
        let lexical_hits = lexical_hits?;
        let interim = rrf_fuse(&[], &vector_hits, &lexical_hits, weights_for(strategy));
        let graph_seed_rows =
            hydrate_graph_seed_rows(&self.pool, &req, &interim, self.assume_app_role).await?;
        let graph_seed_strengths = if req.disable_graph_expansion {
            Vec::new()
        } else {
            interim_graph_seed_strengths(&req.seeds, &interim, &graph_seed_rows, &req.query_text)
        };
        let graph_hits = run_leg(
            req.disable_leg_timeouts,
            "graph",
            GRAPH_BUDGET,
            graph_expansion_leg(
                self.graph.as_ref(),
                &req,
                &graph_seed_strengths,
                &graph_seed_rows,
            ),
        )
        .await?;
        let fusion_started = std::time::Instant::now();
        let mut fused = rrf_fuse(
            &graph_hits,
            &vector_hits,
            &lexical_hits,
            weights_for(strategy),
        );
        fused.truncate(FUSED_CANDIDATE_LIMIT);
        if fused.is_empty() {
            return Ok(Vec::new());
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
        rank_hydrated_hits(&mut hits, &self.ranking_config, &req);
        let final_hits = if req.use_reranker && hits.len() > req.k_final {
            self.rerank_hits(&req, &hits).await?
        } else {
            hits.into_iter().take(req.k_final).collect()
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

        Ok(final_hits)
    }

    async fn vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<LegCandidate>> {
        if req.query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        let Some(workspace_id) = req.scope.workspace_id() else {
            return run_vector_leg(self.vector.as_ref(), req).await;
        };
        let state = self.vector_backend_state(req).await?;
        if state.is_dual_read_active() {
            return self.dual_read_vector_leg(req).await;
        }
        if state.vector_backend == "turbopuffer" {
            if let Some(turbopuffer) = &self.turbopuffer {
                return match run_vector_leg(turbopuffer.as_ref(), req).await {
                    Ok(hits) => Ok(hits),
                    Err(error) if is_turbopuffer_as_of_unsupported(&error) => {
                        tracing::debug!(
                            "Turbopuffer does not support as-of vector queries; using pgvector"
                        );
                        run_vector_leg(self.vector.as_ref(), req).await
                    }
                    Err(error) => Err(error),
                };
            }
            tracing::warn!(
                workspace_id = %workspace_id,
                "workspace is configured for Turbopuffer but no client is configured; falling back to pgvector"
            );
        }

        run_vector_leg(self.vector.as_ref(), req).await
    }

    async fn dual_read_vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<LegCandidate>> {
        let Some(turbopuffer) = &self.turbopuffer else {
            tracing::warn!(
                "workspace is in vector dual-read but no Turbopuffer client is configured"
            );
            return run_vector_leg(self.vector.as_ref(), req).await;
        };

        let pg_future = run_vector_leg(self.vector.as_ref(), req);
        let tp_future = run_vector_leg(turbopuffer.as_ref(), req);
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
            (Err(tp_error), Err(pg_error)) if is_turbopuffer_as_of_unsupported(&tp_error) => {
                Err(pg_error)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    async fn vector_backend_state(&self, req: &RetrievalRequest) -> Result<VectorBackendState> {
        let scope = ScopeContext::new(req.scope.clone());
        let mut conn = ScopedConn::begin(&self.pool, &scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let workspace_id = req.scope.workspace_id().map(|id| id.to_string());
        let row = match workspace_id {
            Some(workspace_id) => {
                sqlx::query_as::<_, (String, String, Option<DateTime<Utc>>)>(
                    r#"
                SELECT vector_backend, vector_backend_state, dual_read_until
                FROM moa.workspace_state
                WHERE workspace_id = $1
                "#,
                )
                .bind(workspace_id)
                .fetch_optional(conn.as_mut())
                .await?
            }
            None => None,
        };
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
        let documents = hits
            .iter()
            .map(|hit| hit.node.name.clone())
            .collect::<Vec<_>>();
        let reranked = self
            .reranker
            .rerank(RERANK_MODEL, &req.query_text, &documents, req.k_final)
            .await?;
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

#[derive(Debug, Clone)]
struct VectorBackendState {
    vector_backend: String,
    vector_backend_state: String,
    dual_read_until: Option<DateTime<Utc>>,
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

fn interim_graph_seed_strengths(
    planner_seeds: &[Uuid],
    interim: &[(Uuid, f64, LegSources)],
    phase_one_rows: &[NodeIndexRow],
    query_text: &str,
) -> Vec<(Uuid, f64)> {
    let mut seen = HashSet::new();
    let mut strengths = Vec::new();
    for seed in planner_seeds {
        if seen.insert(*seed) {
            strengths.push((*seed, 1.0));
        }
    }

    let exact_seed_uids = exact_phase_one_seed_uids(phase_one_rows, query_text);
    let mut phase_one = interim
        .iter()
        .take(PHASE_ONE_GRAPH_SEED_LIMIT)
        .enumerate()
        .map(|(index, (uid, _, _))| (*uid, index, exact_seed_uids.contains(uid)))
        .collect::<Vec<_>>();
    phase_one.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.1.cmp(&right.1)));
    for (index, (uid, _, _)) in phase_one.into_iter().enumerate() {
        if seen.insert(uid) {
            strengths.push((uid, PHASE_ONE_SEED_DECAY.powi(index as i32)));
        }
    }
    strengths
}

async fn hydrate_graph_seed_rows(
    pool: &PgPool,
    req: &RetrievalRequest,
    interim: &[(Uuid, f64, LegSources)],
    assume_app_role: bool,
) -> Result<Vec<NodeIndexRow>> {
    let mut seen = HashSet::new();
    let mut uids = req
        .seeds
        .iter()
        .copied()
        .filter(|uid| seen.insert(*uid))
        .collect::<Vec<_>>();
    uids.extend(
        interim
            .iter()
            .take(PHASE_ONE_GRAPH_SEED_LIMIT)
            .map(|(uid, _, _)| *uid)
            .filter(|uid| seen.insert(*uid)),
    );
    hydrate_nodes(pool, &req.scope, &uids, assume_app_role, req.as_of).await
}

fn exact_phase_one_seed_uids(rows: &[NodeIndexRow], query_text: &str) -> HashSet<Uuid> {
    let query_tokens = normalize_tokens(query_text);
    rows.iter()
        .filter(|row| {
            let name_tokens = normalize_tokens(&row.name);
            !name_tokens.is_empty() && name_tokens.iter().all(|token| query_tokens.contains(token))
        })
        .map(|row| row.uid)
        .collect()
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

fn is_turbopuffer_as_of_unsupported(error: &RetrievalError) -> bool {
    if let RetrievalError::Vector(VectorError::UnsupportedQueryFeature { backend, feature }) = error
    {
        *backend == "turbopuffer" && *feature == "as_of"
    } else {
        false
    }
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
                node,
            })
        })
        .collect()
}

fn rank_hydrated_hits(hits: &mut [RetrievalHit], config: &RankingConfig, req: &RetrievalRequest) {
    match config.mode {
        RankingMode::Legacy => apply_layer_bias(hits),
        RankingMode::FeatureV1 => apply_feature_ranking(hits, config, req),
    }
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
        hit.score = ranker.score(hit.score, max_fused_score, &query_tokens, &hit.node);
        if hit.legs.lexical && !hit.legs.vector && !hit.legs.graph {
            hit.score += config.weights.overlap;
        }
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

pub(crate) fn apply_layer_bias(hits: &mut [RetrievalHit]) {
    for hit in hits.iter_mut() {
        hit.score *= match hit.node.scope.as_str() {
            "user" => 1.3,
            "workspace" => 1.1,
            _ => 1.0,
        };
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.uid.cmp(&right.uid))
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_memory_graph::PiiClass;
    use uuid::Uuid;

    use super::*;
    use crate::retrieval::reranker::{RerankHit, Reranker};

    #[test]
    fn layer_bias_prefers_user_over_workspace_for_matching_scores() {
        let user_uid = Uuid::now_v7();
        let workspace_uid = Uuid::now_v7();
        let mut hits = vec![
            hit(workspace_uid, "workspace", 1.0),
            hit(user_uid, "user", 1.0),
        ];

        apply_layer_bias(&mut hits);

        assert_eq!(hits[0].uid, user_uid);
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn reranker_reorders_candidates_when_enabled() {
        let retriever = HybridRetriever::new(
            PgPool::connect_lazy("postgres://unused")
                .expect("lazy pool construction should not connect"),
            Arc::new(EmptyGraph),
            Arc::new(EmptyVector),
        )
        .with_reranker(Arc::new(ReverseReranker));
        let req = RetrievalRequest {
            seeds: Vec::new(),
            query_text: "deploy provider".to_string(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Global,
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

    #[test]
    fn legacy_mode_preserves_apply_layer_bias_ordering() {
        // Pins: Legacy remains a rollback path for the pre-FeatureV1 ordering.
        let user_uid = Uuid::now_v7();
        let workspace_uid = Uuid::now_v7();
        let mut expected = vec![
            hit(workspace_uid, "workspace", 1.0),
            hit(user_uid, "user", 1.0),
        ];
        apply_layer_bias(&mut expected);

        let mut ranked = vec![
            hit(workspace_uid, "workspace", 1.0),
            hit(user_uid, "user", 1.0),
        ];
        rank_hydrated_hits(
            &mut ranked,
            &RankingConfig {
                mode: RankingMode::Legacy,
                weights: Default::default(),
            },
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "workspace fact".to_string(),
                query_embedding: Vec::new(),
                scope: MemoryScope::Global,
                label_filter: None,
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
        );

        assert_eq!(
            ranked.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
            expected.iter().map(|hit| hit.uid).collect::<Vec<_>>()
        );
        assert_eq!(
            ranked.iter().map(|hit| hit.score).collect::<Vec<_>>(),
            expected.iter().map(|hit| hit.score).collect::<Vec<_>>()
        );
    }

    #[test]
    fn feature_mode_rescues_lexical_non_vector_hit_over_vector_noise() {
        // Pins: FeatureV1 can promote exact lexical hits that vector retrieval missed.
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
                scope: MemoryScope::Global,
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
    fn feature_mode_rescue_skips_graph_lexical_neighbors() {
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
        config.weights.scope_workspace = 0.0;
        let mut hits = vec![graph_lexical_hit, lexical_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "regional network".to_string(),
                query_embedding: Vec::new(),
                scope: MemoryScope::Global,
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
    fn feature_mode_rescues_graph_only_expansion_hit() {
        // Pins: FeatureV1 can promote graph-only expansion hits that vector and lexical missed.
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
        config.weights.scope_workspace = 0.0;
        let mut hits = vec![vector_hit, graph_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "library owner".to_string(),
                query_embedding: Vec::new(),
                scope: MemoryScope::Global,
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
    fn interim_seed_selection_caps_at_configured_limit_and_keeps_ner_strength_on_collision() {
        // Pins: phase-one seeds are capped and planner NER seeds keep strength 1.0 on collision.
        let collision = Uuid::from_u128(1);
        let interim = (1_u128..=(PHASE_ONE_GRAPH_SEED_LIMIT as u128 + 4))
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let strengths = interim_graph_seed_strengths(&[collision], &interim, &[], "");

        assert_eq!(strengths.len(), PHASE_ONE_GRAPH_SEED_LIMIT);
        assert_eq!(strengths[0], (collision, 1.0));
        assert_eq!(strengths[1], (Uuid::from_u128(2), 0.85));
        let (last_uid, last_strength) = strengths.last().expect("last seed should exist");
        assert_eq!(
            *last_uid,
            Uuid::from_u128(PHASE_ONE_GRAPH_SEED_LIMIT as u128)
        );
        assert!(
            (*last_strength - PHASE_ONE_SEED_DECAY.powi((PHASE_ONE_GRAPH_SEED_LIMIT - 1) as i32))
                .abs()
                < f64::EPSILON
        );
        assert!(
            strengths
                .iter()
                .all(|(uid, strength)| *uid != collision || *strength == 1.0)
        );
    }

    #[test]
    fn graph_seed_selection_uses_phase_one_when_planner_seeds_empty() {
        // Pins: graph expansion can still run when NER finds no planner seeds.
        let first = Uuid::from_u128(10);
        let second = Uuid::from_u128(11);
        let interim = vec![
            (first, 1.0, LegSources::default()),
            (second, 0.5, LegSources::default()),
        ];

        let strengths = interim_graph_seed_strengths(&[], &interim, &[], "");

        assert_eq!(strengths, vec![(first, 1.0), (second, 0.85)]);
    }

    #[test]
    fn interim_seed_selection_promotes_exact_phase_one_subject_match() {
        // Pins: graph expansion starts from the exact entity mention before same-shape siblings.
        let sibling = Uuid::from_u128(10);
        let exact = Uuid::from_u128(11);
        let interim = vec![
            (sibling, 1.0, LegSources::default()),
            (exact, 0.9, LegSources::default()),
        ];
        let rows = vec![
            node_row(sibling, "audit-shipper-dep-0-4-0"),
            node_row(exact, "audit-shipper-dep-0-0-0"),
        ];

        let strengths = interim_graph_seed_strengths(
            &[],
            &interim,
            &rows,
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
        );

        assert_eq!(strengths, vec![(exact, 1.0), (sibling, 0.85)]);
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

    fn hit(uid: Uuid, scope: &str, score: f64) -> RetrievalHit {
        RetrievalHit {
            uid,
            score,
            legs: LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
            node: NodeIndexRow {
                uid,
                label: NodeLabel::Fact,
                workspace_id: Some("workspace".to_string()),
                user_id: None,
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

    fn node_row(uid: Uuid, name: &str) -> NodeIndexRow {
        NodeIndexRow {
            uid,
            label: NodeLabel::Fact,
            workspace_id: Some("workspace".to_string()),
            user_id: None,
            scope: "workspace".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: Utc::now(),
            properties_summary: None,
            last_accessed_at: Utc::now(),
            quality_score: 0.5,
        }
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
        ) -> Result<Vec<RerankHit>> {
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

    struct EmptyVector;

    #[async_trait]
    impl VectorStore for EmptyVector {
        fn backend(&self) -> &'static str {
            "empty"
        }

        fn dimension(&self) -> usize {
            1024
        }

        async fn upsert(
            &self,
            _items: &[moa_memory_vector::VectorItem],
        ) -> std::result::Result<(), VectorError> {
            unreachable!("not used by retrieval tests")
        }

        async fn knn(
            &self,
            _query: &moa_memory_vector::VectorQuery,
        ) -> std::result::Result<Vec<moa_memory_vector::VectorMatch>, VectorError> {
            Ok(Vec::new())
        }

        async fn delete(&self, _uids: &[Uuid]) -> std::result::Result<(), VectorError> {
            unreachable!("not used by retrieval tests")
        }
    }
}
