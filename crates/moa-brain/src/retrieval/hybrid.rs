//! Production hybrid graph-memory retriever.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, ScopeContext, ScopedConn};
use moa_memory_graph::{GraphError, GraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_vector::{Error as VectorError, TurbopufferStore, VectorStore};
use secrecy::SecretString;
use sqlx::PgPool;
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, LegCandidate, VECTOR_BUDGET,
    VECTOR_WEIGHT, bump_last_accessed, graph_leg, hydrate_nodes, lexical_leg, rrf_fuse, timed_leg,
    vector_leg as run_vector_leg,
};
use crate::retrieval::reranker::{CohereReranker, NoopReranker, Reranker};

const RERANK_MODEL: &str = "rerank-v4.0-fast";
const FUSED_CANDIDATE_LIMIT: usize = 25;

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
    /// Fused retrieval score after layer-priority bias.
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
    assume_app_role: bool,
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
            assume_app_role: false,
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
            .or_else(|_| std::env::var("MOA_COHERE_API_KEY"))
            .map(|api_key| {
                Arc::new(CohereReranker::new(SecretString::from(api_key))) as Arc<dyn Reranker>
            })
            .unwrap_or_else(|_| Arc::new(NoopReranker));
        let turbopuffer = TurbopufferStore::from_env().ok().map(Arc::new);
        Self::new(pool, graph, vector)
            .with_turbopuffer(turbopuffer)
            .with_reranker(reranker)
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
        let graph = self.graph.as_ref();
        let graph_future = timed_leg("graph", GRAPH_BUDGET, graph_leg(graph, &req));
        let vector_future = timed_leg("vector", VECTOR_BUDGET, self.vector_leg(&req));
        let lexical_future = timed_leg(
            "lexical",
            LEXICAL_BUDGET,
            lexical_leg(&self.pool, &req, self.assume_app_role),
        );
        let (graph_result, vector_result, lexical_result) =
            tokio::join!(graph_future, vector_future, lexical_future);

        let graph_hits = leg_or_empty("graph", graph_result)?;
        let vector_hits = leg_or_empty("vector", vector_result)?;
        let lexical_hits = leg_or_empty("lexical", lexical_result)?;
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
        let nodes =
            hydrate_nodes(&self.pool, &req.scope, &fused_uids, self.assume_app_role).await?;
        let mut hits = build_hits(fused, nodes);
        apply_layer_bias(&mut hits);
        let final_hits = if req.use_reranker && hits.len() > req.k_final {
            self.rerank_hits(&req, &hits).await?
        } else {
            hits.into_iter().take(req.k_final).collect()
        };

        let touched_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
        let pool = self.pool.clone();
        let scope = req.scope.clone();
        let assume_app_role = self.assume_app_role;
        tokio::spawn(async move {
            if let Err(error) = bump_last_accessed(pool, scope, touched_uids, assume_app_role).await
            {
                tracing::debug!(error = %error, "failed to bump graph-memory access timestamps");
            }
        });

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
                return run_vector_leg(turbopuffer.as_ref(), req).await;
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
        };
        let first = hit(Uuid::now_v7(), "workspace", 2.0);
        let second = hit(Uuid::now_v7(), "workspace", 1.0);

        let reranked = retriever
            .rerank_hits(&req, &[first.clone(), second.clone()])
            .await
            .expect("rerank should succeed");

        assert_eq!(reranked, vec![second]);
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
            },
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
        ) -> std::result::Result<Vec<NodeIndexRow>, GraphError> {
            Ok(Vec::new())
        }

        async fn lookup_seeds(
            &self,
            _name: &str,
            _limit: i64,
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
