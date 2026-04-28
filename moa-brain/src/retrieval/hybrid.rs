//! Production hybrid graph-memory retriever.

use std::collections::HashMap;
use std::sync::Arc;

use moa_core::MemoryScope;
use moa_memory_graph::{GraphError, GraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_vector::{Error as VectorError, VectorStore};
use secrecy::SecretString;
use sqlx::PgPool;
use uuid::Uuid;

use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, VECTOR_BUDGET, VECTOR_WEIGHT,
    bump_last_accessed, graph_leg, hydrate_nodes, lexical_leg, rrf_fuse, timed_leg, vector_leg,
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
        Self::new(pool, graph, vector).with_reranker(reranker)
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

        let graph = self.graph.as_ref();
        let vector = self.vector.as_ref();
        let graph_future = timed_leg("graph", GRAPH_BUDGET, graph_leg(graph, &req));
        let vector_future = timed_leg("vector", VECTOR_BUDGET, vector_leg(vector, &req));
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
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT),
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
