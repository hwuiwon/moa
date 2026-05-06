//! Individual retrieval legs and reciprocal-rank fusion helpers.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use moa_core::{MemoryScope, ScopeContext, ScopedConn};
use moa_memory_graph::{GraphStore, NodeIndexRow, PiiClass};
use moa_memory_vector::{VectorQuery, VectorStore};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::retrieval::hybrid::{LegSources, Result, RetrievalRequest};

/// Reciprocal-rank fusion denominator offset.
pub const RRF_K: f64 = 60.0;
/// Default graph-leg fusion weight.
pub const GRAPH_WEIGHT: f64 = 1.2;
/// Default vector-leg fusion weight.
pub const VECTOR_WEIGHT: f64 = 1.0;
/// Default lexical-leg fusion weight.
pub const LEXICAL_WEIGHT: f64 = 0.8;
/// Graph traversal leg budget.
pub const GRAPH_BUDGET: Duration = Duration::from_millis(250);
/// Vector KNN leg budget.
pub const VECTOR_BUDGET: Duration = Duration::from_millis(250);
/// Lexical tsvector leg budget.
pub const LEXICAL_BUDGET: Duration = Duration::from_secs(1);

const GRAPH_HOPS: u8 = 3;
const VECTOR_LIMIT: usize = 20;
const LEXICAL_LIMIT: i64 = 20;

/// One ranked candidate from an individual retrieval leg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegCandidate {
    /// Candidate node uid.
    pub uid: Uuid,
    /// RRF contribution for this leg.
    pub score: f64,
}

/// Runs the graph traversal leg from planner-supplied seed nodes.
pub async fn graph_leg(
    graph: &dyn GraphStore,
    req: &RetrievalRequest,
) -> Result<Vec<LegCandidate>> {
    if req.seeds.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for seed in &req.seeds {
        let nodes = graph.neighbors(*seed, GRAPH_HOPS, None).await?;
        for node in nodes {
            if seen.insert(node.uid) {
                candidates.push(node.uid);
            }
        }
    }
    Ok(rank_uids(candidates))
}

/// Runs the vector KNN leg.
pub async fn vector_leg(
    vector: &dyn VectorStore,
    req: &RetrievalRequest,
) -> Result<Vec<LegCandidate>> {
    if req.query_embedding.is_empty() {
        return Ok(Vec::new());
    }

    let label_filter = req.label_filter.as_ref().map(|labels| {
        labels
            .iter()
            .map(|label| label.as_str().to_string())
            .collect::<Vec<_>>()
    });
    let hits = vector
        .knn(&VectorQuery {
            workspace_id: req
                .scope
                .workspace_id()
                .map(|workspace_id| workspace_id.to_string()),
            embedding: req.query_embedding.clone(),
            k: VECTOR_LIMIT,
            label_filter,
            max_pii_class: req.max_pii_class.as_str().to_string(),
            include_global: true,
        })
        .await?;
    Ok(rank_uids(hits.into_iter().map(|hit| hit.uid).collect()))
}

/// Runs the Postgres tsvector lexical leg over `moa.node_index`.
pub async fn lexical_leg(
    pool: &PgPool,
    req: &RetrievalRequest,
    assume_app_role: bool,
) -> Result<Vec<LegCandidate>> {
    if req.query_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = begin_scoped(pool, &req.scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND name_tsv @@ plainto_tsquery('simple', "#,
    );
    builder.push_bind(&req.query_text);
    builder.push(
        r#")
          AND CASE pii_class
                WHEN 'none' THEN 0
                WHEN 'pii' THEN 1
                WHEN 'phi' THEN 2
                WHEN 'restricted' THEN 3
                ELSE 4
              END <= "#,
    );
    builder.push_bind(pii_rank(req.max_pii_class));
    if let Some(labels) = req
        .label_filter
        .as_ref()
        .filter(|labels| !labels.is_empty())
    {
        let label_values = labels
            .iter()
            .map(|label| label.as_str().to_string())
            .collect::<Vec<_>>();
        builder.push(" AND label = ANY(");
        builder.push_bind(label_values);
        builder.push(")");
    }
    builder.push(
        r#"
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', "#,
    );
    builder.push_bind(&req.query_text);
    builder.push(")) DESC, last_accessed_at DESC LIMIT ");
    builder.push_bind(LEXICAL_LIMIT);

    let rows = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(conn.as_mut())
        .await?;
    conn.commit().await?;
    if !rows.is_empty() {
        return Ok(rank_uids(rows));
    }

    let terms = lexical_fallback_terms(&req.query_text);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    lexical_fallback_leg(pool, req, assume_app_role, &terms).await
}

async fn lexical_fallback_leg(
    pool: &PgPool,
    req: &RetrievalRequest,
    assume_app_role: bool,
    terms: &[String],
) -> Result<Vec<LegCandidate>> {
    let mut conn = begin_scoped(pool, &req.scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        WITH terms(term) AS (SELECT unnest("#,
    );
    builder.push_bind(terms);
    builder.push(
        r#"::text[]))
        SELECT node.uid
        FROM moa.node_index AS node
        CROSS JOIN LATERAL (
            SELECT COUNT(*) AS match_count
            FROM terms
            WHERE LOWER(node.name || ' ' || COALESCE(node.properties_summary::text, ''))
                  LIKE '%' || terms.term || '%'
        ) AS matches
        WHERE node.valid_to IS NULL
          AND matches.match_count > 0
          AND CASE node.pii_class
                WHEN 'none' THEN 0
                WHEN 'pii' THEN 1
                WHEN 'phi' THEN 2
                WHEN 'restricted' THEN 3
                ELSE 4
              END <= "#,
    );
    builder.push_bind(pii_rank(req.max_pii_class));
    if let Some(labels) = req
        .label_filter
        .as_ref()
        .filter(|labels| !labels.is_empty())
    {
        let label_values = labels
            .iter()
            .map(|label| label.as_str().to_string())
            .collect::<Vec<_>>();
        builder.push(" AND node.label = ANY(");
        builder.push_bind(label_values);
        builder.push(")");
    }
    builder.push(
        r#"
        ORDER BY matches.match_count DESC, node.last_accessed_at DESC
        LIMIT "#,
    );
    builder.push_bind(LEXICAL_LIMIT);

    let rows = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(conn.as_mut())
        .await?;
    conn.commit().await?;
    Ok(rank_uids(rows))
}

fn lexical_fallback_terms(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "about", "does", "from", "have", "into", "news", "that", "the", "this", "what", "when",
        "where", "which", "with",
    ];
    let mut terms = Vec::new();
    for raw in
        query.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'))
    {
        let term = raw.trim_matches('.').to_ascii_lowercase();
        if term.len() < 3 || STOPWORDS.contains(&term.as_str()) || terms.contains(&term) {
            continue;
        }
        terms.push(term);
    }
    terms
}

/// Hydrates fused candidate uids through the sidecar, preserving RLS.
pub async fn hydrate_nodes(
    pool: &PgPool,
    scope: &MemoryScope,
    uids: &[Uuid],
    assume_app_role: bool,
) -> Result<Vec<NodeIndexRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = begin_scoped(pool, scope, assume_app_role).await?;
    let rows = sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE uid = ANY($1)
          AND valid_to IS NULL
        "#,
    )
    .bind(uids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(rows)
}

/// Updates `last_accessed_at` for retrieved nodes in a scoped background transaction.
pub async fn bump_last_accessed(
    pool: PgPool,
    scope: MemoryScope,
    uids: Vec<Uuid>,
    assume_app_role: bool,
) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }

    let mut conn = begin_scoped(&pool, &scope, assume_app_role).await?;
    sqlx::query("UPDATE moa.node_index SET last_accessed_at = now() WHERE uid = ANY($1)")
        .bind(&uids)
        .execute(conn.as_mut())
        .await?;
    conn.commit().await?;
    Ok(())
}

/// Fuses ranked leg candidates using weighted reciprocal-rank fusion.
#[must_use]
pub fn rrf_fuse(
    graph: &[LegCandidate],
    vector: &[LegCandidate],
    lexical: &[LegCandidate],
    weights: (f64, f64, f64),
) -> Vec<(Uuid, f64, LegSources)> {
    let mut scores = HashMap::<Uuid, (f64, LegSources)>::new();
    add_leg_scores(&mut scores, graph, weights.0, |sources| {
        sources.graph = true;
    });
    add_leg_scores(&mut scores, vector, weights.1, |sources| {
        sources.vector = true;
    });
    add_leg_scores(&mut scores, lexical, weights.2, |sources| {
        sources.lexical = true;
    });

    let mut fused = scores
        .into_iter()
        .map(|(uid, (score, sources))| (uid, score, sources))
        .collect::<Vec<_>>();
    sort_fused(&mut fused);
    fused
}

/// Sorts fused candidates by descending score with uid as a deterministic tie-breaker.
pub fn sort_fused(fused: &mut [(Uuid, f64, LegSources)]) {
    fused.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
}

/// Starts a scoped connection for sidecar reads.
pub async fn begin_scoped<'a>(
    pool: &'a PgPool,
    scope: &MemoryScope,
    assume_app_role: bool,
) -> Result<ScopedConn<'a>> {
    let scope_context = ScopeContext::from(scope.clone());
    let mut conn = ScopedConn::begin(pool, &scope_context).await?;
    if assume_app_role {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
    }
    Ok(conn)
}

/// Measures one leg future and logs its elapsed time.
pub async fn timed_leg<T, F>(
    name: &'static str,
    budget: Duration,
    future: F,
) -> std::result::Result<Result<T>, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = Result<T>>,
{
    let started = Instant::now();
    let result = tokio::time::timeout(budget, future).await;
    let elapsed = started.elapsed();
    metrics::histogram!("moa_retrieval_leg_seconds", "leg" => name).record(elapsed.as_secs_f64());
    tracing::debug!(
        leg = name,
        elapsed_ms = elapsed.as_millis(),
        budget_ms = budget.as_millis(),
        timed_out = result.is_err(),
        "hybrid retrieval leg finished"
    );
    result
}

fn rank_uids(uids: Vec<Uuid>) -> Vec<LegCandidate> {
    uids.into_iter()
        .enumerate()
        .map(|(rank, uid)| LegCandidate {
            uid,
            score: 1.0 / (RRF_K + rank as f64 + 1.0),
        })
        .collect()
}

fn add_leg_scores(
    scores: &mut HashMap<Uuid, (f64, LegSources)>,
    candidates: &[LegCandidate],
    weight: f64,
    mark: impl Fn(&mut LegSources),
) {
    for candidate in candidates {
        let entry = scores
            .entry(candidate.uid)
            .or_insert_with(|| (0.0, LegSources::default()));
        entry.0 += candidate.score * weight;
        mark(&mut entry.1);
    }
}

fn pii_rank(class: PiiClass) -> i32 {
    match class {
        PiiClass::None => 0,
        PiiClass::Pii => 1,
        PiiClass::Phi => 2,
        PiiClass::Restricted => 3,
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{
        GRAPH_WEIGHT, LEXICAL_WEIGHT, LegCandidate, VECTOR_WEIGHT, lexical_fallback_terms, rrf_fuse,
    };

    #[test]
    fn rrf_fuse_tracks_all_contributing_legs() {
        let shared = Uuid::now_v7();
        let graph_only = Uuid::now_v7();
        let lexical_only = Uuid::now_v7();

        let fused = rrf_fuse(
            &[
                LegCandidate {
                    uid: graph_only,
                    score: 1.0 / 61.0,
                },
                LegCandidate {
                    uid: shared,
                    score: 1.0 / 62.0,
                },
            ],
            &[LegCandidate {
                uid: shared,
                score: 1.0 / 61.0,
            }],
            &[LegCandidate {
                uid: lexical_only,
                score: 1.0 / 61.0,
            }],
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT),
        );

        let shared_hit = fused
            .iter()
            .find(|(uid, _, _)| *uid == shared)
            .expect("shared hit should be present");
        assert!(shared_hit.2.graph);
        assert!(shared_hit.2.vector);
        assert!(!shared_hit.2.lexical);
        assert_eq!(fused[0].0, shared);
    }

    #[test]
    fn lexical_fallback_terms_keep_article_ids_and_drop_question_words() {
        let terms = lexical_fallback_terms("What is news_article_001 about?");

        assert_eq!(terms, vec!["news_article_001"]);
    }
}
