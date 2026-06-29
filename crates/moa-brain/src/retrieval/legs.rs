//! Individual retrieval legs and reciprocal-rank fusion helpers.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_core::RlsContext;
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, GraphExpansionHit, GraphStore, NodeIndexRow, NodeLabel, PiiClass,
    push_validity_filter,
};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{TurbopufferStore, TurbopufferTextQuery, VectorQuery, VectorStore};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::retrieval::hybrid::{LegSources, LineageContext, Result, RetrievalRequest};
use crate::retrieval::ranking::normalize_tokens;

/// Reciprocal-rank fusion denominator offset.
pub const RRF_K: f64 = 60.0;
/// Default graph-leg fusion weight.
pub const GRAPH_WEIGHT: f64 = 0.4;
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
const GRAPH_EXPANSION_DECAY: f64 = 0.5;
const GRAPH_TEMPORAL_HALF_LIFE_DAYS: f64 = 30.0;
const VECTOR_LIMIT: usize = 20;
const LEXICAL_LIMIT: i64 = 20;
const LEXICAL_LIMIT_USIZE: usize = 20;

/// One ranked candidate from an individual retrieval leg.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegCandidate {
    /// Candidate node uid.
    pub uid: Uuid,
    /// RRF contribution for this leg.
    pub score: f64,
}

/// Runs the graph expansion leg from weighted planner and phase-one seeds.
pub async fn graph_expansion_leg(
    graph: &dyn GraphStore,
    req: &RetrievalRequest,
    seed_strengths: &[(Uuid, f64)],
    seed_rows: &[NodeIndexRow],
) -> Result<Vec<LegCandidate>> {
    if seed_strengths.is_empty() {
        return Ok(Vec::new());
    }

    let seeds = seed_strengths
        .iter()
        .map(|(seed, _)| *seed)
        .collect::<Vec<_>>();
    let strengths = seed_strengths
        .iter()
        .copied()
        .collect::<HashMap<Uuid, f64>>();
    let hits = graph.expand_seeds(&seeds, GRAPH_HOPS, req.as_of).await?;
    let seed_candidates = exact_seed_candidates(
        seed_rows,
        &strengths,
        &req.query_text,
        req.label_filter.as_deref(),
    );
    let candidates = merge_ordered_uids(
        seed_candidates,
        score_expansion(&hits, &strengths, req.as_of, req.label_filter.as_deref()),
    );
    Ok(rank_uids(candidates))
}

fn label_allowed_by_filter(label_filter: Option<&[NodeLabel]>, label: &NodeLabel) -> bool {
    label_filter.is_none_or(|labels| labels.is_empty() || labels.contains(label))
}

/// Scores graph expansion paths by seed strength, edge weight, and hop decay.
///
/// Edge weights are explicit for all current `EdgeLabel` variants:
/// `RELATES_TO`, `DEPENDS_ON`, `OWNED_BY`, `DERIVED_FROM`, `MENTIONED_IN`,
/// `CAUSED`, `LEARNED_FROM`, and `APPLIES_TO` carry weight 1.0; `CONTRADICTS` carries
/// 0.0; `SUPERSEDES` carries 0.6 only for as-of retrieval. `supersede_node`
/// creates `new -[:SUPERSEDES]-> old`, so historical as-of expansion can cross
/// from an active replacement seed to its superseded predecessor.
#[must_use]
pub fn score_expansion(
    hits: &[GraphExpansionHit],
    strengths: &HashMap<Uuid, f64>,
    as_of: Option<DateTime<Utc>>,
    label_filter: Option<&[NodeLabel]>,
) -> Vec<Uuid> {
    let entity_explicitly_allowed =
        label_filter.is_some_and(|labels| labels.contains(&NodeLabel::Entity));
    let mut activation_by_uid = HashMap::<Uuid, (f64, NodeLabel)>::new();

    for hit in hits {
        if hit.uid == hit.seed {
            continue;
        }
        if hit.label == NodeLabel::Entity && !entity_explicitly_allowed {
            continue;
        }
        if !label_allowed_by_filter(label_filter, &hit.label) {
            continue;
        }
        let Some(seed_strength) = strengths.get(&hit.seed).copied() else {
            continue;
        };
        let path_weight = path_weight(&hit.edges, as_of);
        if path_weight <= 0.0 {
            continue;
        }
        let activation = seed_strength
            * path_weight
            * GRAPH_EXPANSION_DECAY.powi(i32::from(hit.hop))
            * temporal_coherence(hit.seed_valid_from, hit.valid_from);
        if activation <= 0.0 {
            continue;
        }
        let entry = activation_by_uid.entry(hit.uid).or_insert((0.0, hit.label));
        entry.0 += activation;
    }

    let mut scored = activation_by_uid
        .into_iter()
        .map(|(uid, (activation, _))| (uid, activation))
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    scored.into_iter().map(|(uid, _)| uid).collect()
}

fn path_weight(edges: &[EdgeLabel], as_of: Option<DateTime<Utc>>) -> f64 {
    if edges.is_empty() {
        return 0.0;
    }
    edges.iter().map(|edge| edge_weight(*edge, as_of)).product()
}

fn edge_weight(edge: EdgeLabel, as_of: Option<DateTime<Utc>>) -> f64 {
    match edge {
        EdgeLabel::RelatesTo
        | EdgeLabel::DependsOn
        | EdgeLabel::OwnedBy
        | EdgeLabel::DerivedFrom
        | EdgeLabel::Contains
        | EdgeLabel::MentionedIn
        | EdgeLabel::MemberOf
        | EdgeLabel::Caused
        | EdgeLabel::LearnedFrom
        | EdgeLabel::AppliesTo => 1.0,
        EdgeLabel::Supersedes if as_of.is_some() => 0.6,
        EdgeLabel::Supersedes | EdgeLabel::Contradicts => 0.0,
    }
}

fn temporal_coherence(seed_valid_from: DateTime<Utc>, hit_valid_from: DateTime<Utc>) -> f64 {
    let distance_days = seed_valid_from
        .signed_duration_since(hit_valid_from)
        .num_seconds()
        .unsigned_abs() as f64
        / 86_400.0;
    2.0_f64.powf(-(distance_days / GRAPH_TEMPORAL_HALF_LIFE_DAYS))
}

fn exact_seed_candidates(
    seed_rows: &[NodeIndexRow],
    strengths: &HashMap<Uuid, f64>,
    query_text: &str,
    label_filter: Option<&[NodeLabel]>,
) -> Vec<Uuid> {
    let entity_explicitly_allowed =
        label_filter.is_some_and(|labels| labels.contains(&NodeLabel::Entity));
    let query_tokens = normalize_tokens(query_text);
    let mut candidates = seed_rows
        .iter()
        .filter(|row| strengths.contains_key(&row.uid))
        .filter(|row| row.label != NodeLabel::Entity || entity_explicitly_allowed)
        .filter(|row| label_allowed_by_filter(label_filter, &row.label))
        .filter(|row| {
            let name_tokens = normalize_tokens(&row.name);
            !name_tokens.is_empty() && name_tokens.iter().all(|token| query_tokens.contains(token))
        })
        .map(|row| {
            (
                row.uid,
                strengths.get(&row.uid).copied().unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    candidates.into_iter().map(|(uid, _)| uid).collect()
}

fn merge_ordered_uids(first: Vec<Uuid>, second: Vec<Uuid>) -> Vec<Uuid> {
    let mut seen = std::collections::HashSet::new();
    first
        .into_iter()
        .chain(second)
        .filter(|uid| seen.insert(*uid))
        .collect()
}

/// Runs the vector KNN leg.
pub async fn vector_leg(
    vector: &dyn VectorStore,
    req: &RetrievalRequest,
) -> Result<Vec<LegCandidate>> {
    if req.query_embedding.is_empty() {
        return Ok(Vec::new());
    }

    let hits = vector
        .knn(&VectorQuery {
            embedding: req.query_embedding.clone(),
            k: VECTOR_LIMIT,
            label_filter: Some(effective_label_filter_values(req.label_filter.as_deref())),
            max_pii_class: req.max_pii_class.as_str().to_string(),
            include_global: true,
            as_of: req.as_of,
        })
        .await?;
    Ok(rank_uids(hits.into_iter().map(|hit| hit.uid).collect()))
}

/// Runs the Turbopuffer BM25 lexical leg.
pub async fn turbopuffer_bm25_leg(
    turbopuffer: &TurbopufferStore,
    req: &RetrievalRequest,
) -> Result<Vec<LegCandidate>> {
    if req.query_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    let hits = turbopuffer
        .bm25(&TurbopufferTextQuery {
            query_text: req.query_text.clone(),
            k: LEXICAL_LIMIT_USIZE,
            label_filter: Some(effective_label_filter_values(req.label_filter.as_deref())),
            max_pii_class: req.max_pii_class.as_str().to_string(),
            include_global: true,
        })
        .await?;
    Ok(rank_uids(hits.into_iter().map(|hit| hit.uid).collect()))
}

/// Runs the Postgres tsvector lexical leg over `moa.node_index`.
///
/// The query matches ANY extracted term (plus stemmed variants) and orders
/// by `ts_rank`, because conversational queries rarely contain every token
/// of a stored fact name.
pub async fn lexical_leg(
    pool: &PgPool,
    req: &RetrievalRequest,
    assume_app_role: bool,
) -> Result<Vec<LegCandidate>> {
    let tsquery = lexical_or_tsquery(&req.query_text);
    if tsquery.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = begin_scoped(pool, &req.scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE "#,
    );
    push_validity_filter(&mut builder, None, req.as_of);
    builder.push(
        r#"
          AND name_tsv @@ to_tsquery('simple', "#,
    );
    builder.push_bind(tsquery.clone());
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
    builder.push(" AND label = ANY(");
    builder.push_bind(effective_label_filter_values(req.label_filter.as_deref()));
    builder.push(")");
    builder.push(
        r#"
        ORDER BY ts_rank(name_tsv, to_tsquery('simple', "#,
    );
    builder.push_bind(tsquery);
    builder.push(")) DESC, ");
    push_accessed_ordering(&mut builder, None, req.ranking_reference_time);
    builder.push(" LIMIT ");
    builder.push_bind(LEXICAL_LIMIT);

    let rows = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(conn.as_mut())
        .await?;
    conn.commit().await?;

    let terms = lexical_fallback_terms(&req.query_text);
    if terms.is_empty() {
        return Ok(rank_uids(rows));
    }
    if !should_run_lexical_fallback(!rows.is_empty(), &terms) {
        return Ok(rank_uids(rows));
    }
    let fallback = lexical_fallback_leg(pool, req, assume_app_role, &terms).await?;
    let fallback_uids = fallback
        .into_iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    let uids = if prefer_lexical_fallback_first(&terms) {
        merge_ordered_uids(fallback_uids, rows)
    } else {
        merge_ordered_uids(rows, fallback_uids)
    };
    Ok(rank_uids(uids))
}

/// Builds an OR `to_tsquery` input from query terms and their stems.
fn lexical_or_tsquery(query: &str) -> String {
    let stemmer = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English);
    let mut variants = Vec::new();
    for term in lexical_fallback_terms(query) {
        let stemmed = stemmer.stem(&term).into_owned();
        for variant in [term, stemmed] {
            if !variant.is_empty() && !variants.contains(&variant) {
                variants.push(variant);
            }
        }
    }
    variants
        .into_iter()
        .map(|term| format!("'{}'", term.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(" | ")
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
        WHERE "#,
    );
    push_validity_filter(&mut builder, Some("node"), req.as_of);
    builder.push(
        r#"
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
    builder.push(" AND node.label = ANY(");
    builder.push_bind(effective_label_filter_values(req.label_filter.as_deref()));
    builder.push(")");
    builder.push(
        r#"
        ORDER BY matches.match_count DESC, "#,
    );
    push_accessed_ordering(&mut builder, Some("node"), req.ranking_reference_time);
    builder.push(" LIMIT ");
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
    if terms
        .iter()
        .any(|term| term == "prefer" || term == "format" || term == "style")
    {
        push_term(&mut terms, "response_style");
    }
    if terms.iter().any(|term| term == "private") && terms.iter().any(|term| term == "repository") {
        push_term(&mut terms, "private_repository");
    }
    if terms.iter().any(|term| term == "runbook")
        && terms
            .iter()
            .any(|term| term == "required" || term == "require")
    {
        push_term(&mut terms, "require_runbook");
    }
    terms
}

fn push_term(terms: &mut Vec<String>, term: &str) {
    if !terms.iter().any(|existing| existing == term) {
        terms.push(term.to_string());
    }
}

fn should_run_lexical_fallback(primary_found: bool, terms: &[String]) -> bool {
    !primary_found || terms.iter().any(|term| is_structured_lookup_term(term))
}

fn prefer_lexical_fallback_first(terms: &[String]) -> bool {
    terms.iter().any(|term| {
        matches!(
            term.as_str(),
            "private_repository" | "response_style" | "require_runbook"
        ) || (term.contains('-') && term.chars().any(|ch| ch.is_ascii_digit()))
    })
}

fn is_structured_lookup_term(term: &str) -> bool {
    matches!(
        term,
        "private_repository" | "response_style" | "require_runbook"
    ) || term.contains('_')
        || (term.contains('-') && term.chars().any(|ch| ch.is_ascii_digit()))
}

fn push_accessed_ordering(
    builder: &mut QueryBuilder<'_, Postgres>,
    table_alias: Option<&str>,
    _reference_time: Option<DateTime<Utc>>,
) {
    if let Some(alias) = table_alias {
        builder.push(alias);
        builder.push(".");
    }
    builder.push("last_accessed_at");
    builder.push(" DESC, ");
    if let Some(alias) = table_alias {
        builder.push(alias);
        builder.push(".");
    }
    builder.push("uid ASC");
}

/// Hydrates fused candidate uids through the sidecar, preserving RLS.
pub async fn hydrate_nodes(
    pool: &PgPool,
    scope: &MemoryScope,
    uids: &[Uuid],
    assume_app_role: bool,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<NodeIndexRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = begin_scoped(pool, scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at,
               COALESCE(quality_score, 0.5) AS quality_score
        FROM moa.node_index
        WHERE uid = ANY("#,
    );
    builder.push_bind(uids);
    builder.push(") AND ");
    push_validity_filter(&mut builder, None, as_of);
    let rows = builder
        .build_query_as::<NodeIndexRow>()
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
    let mut builder = QueryBuilder::<Postgres>::new(
        "UPDATE moa.node_index SET last_accessed_at = now() WHERE uid = ANY(",
    );
    builder.push_bind(&uids);
    builder.push(")");
    match &scope {
        MemoryScope::Tenant { tenant_id } => {
            builder.push(" AND storage_partition_id = ");
            builder.push_bind(tenant_id.to_string());
            builder.push(" AND contact_id IS NULL");
        }
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        } => {
            builder.push(" AND storage_partition_id = ");
            builder.push_bind(tenant_id.to_string());
            builder.push(" AND contact_id = ");
            builder.push_bind(contact_id.0);
        }
    }
    builder.build().execute(conn.as_mut()).await?;
    conn.commit().await?;
    Ok(())
}

/// Writes narrow retrieval lineage rows for later quality-score computation.
pub async fn write_retrieval_lineage(
    pool: PgPool,
    scope: MemoryScope,
    lineage: LineageContext,
    ranked_uids: Vec<Uuid>,
    retrieved_at: DateTime<Utc>,
    assume_app_role: bool,
) -> Result<()> {
    if ranked_uids.is_empty() {
        return Ok(());
    }
    let tenant_id = scope.tenant_id();
    let contact_id = scope.contact_id();
    let mut conn = begin_scoped(&pool, &scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO moa.retrieval_lineage \
         (tenant_id, contact_id, storage_partition_id, user_id, session_id, turn_seq, turn_id, uid, rank, retrieved_at) ",
    );
    builder.push_values(ranked_uids.iter().enumerate(), |mut row, (index, uid)| {
        row.push_bind(tenant_id.0)
            .push_bind(contact_id.map(|id| id.0))
            .push_bind(tenant_id.to_string())
            .push_bind(contact_id.map(|id| id.to_string()))
            .push_bind(lineage.session_id.0)
            .push_bind(lineage.turn_seq)
            .push_bind(lineage.turn_id.map(|turn_id| turn_id.0))
            .push_bind(*uid)
            .push_bind(i32::try_from(index + 1).unwrap_or(i32::MAX))
            .push_bind(retrieved_at);
    });
    builder.build().execute(conn.as_mut()).await?;
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
    let scope_context = RlsContext::from(scope.clone());
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

fn effective_label_filter_values(label_filter: Option<&[NodeLabel]>) -> Vec<String> {
    match label_filter.filter(|labels| !labels.is_empty()) {
        Some(labels) => labels
            .iter()
            .map(|label| label.as_str().to_string())
            .collect(),
        None => [
            NodeLabel::Concept,
            NodeLabel::Decision,
            NodeLabel::Incident,
            NodeLabel::Lesson,
            NodeLabel::Fact,
            NodeLabel::Source,
        ]
        .into_iter()
        .map(|label| label.as_str().to_string())
        .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::TimeZone;
    use moa_core::TenantId;
    use moa_memory_graph::{EdgeLabel, GraphExpansionHit, NodeIndexRow, NodeLabel, PiiClass};
    use moa_memory_types::MemoryScope;
    use moa_memory_vector::{Error as VectorError, VectorItem, VectorMatch, VectorQuery};
    use sqlx::PgConnection;
    use uuid::Uuid;

    use super::{
        GRAPH_WEIGHT, LEXICAL_WEIGHT, LegCandidate, RetrievalRequest, VECTOR_WEIGHT,
        exact_seed_candidates, lexical_fallback_terms, lexical_or_tsquery, merge_ordered_uids,
        prefer_lexical_fallback_first, rrf_fuse, score_expansion, should_run_lexical_fallback,
        vector_leg,
    };

    #[test]
    fn lexical_or_tsquery_handles_empty_stopword_and_quoted_input() {
        // Pins: empty and all-stopword queries produce an empty tsquery, which
        // makes the lexical leg short-circuit without hitting the database; and
        // apostrophes in the query never leak into the generated to_tsquery text.
        assert_eq!(lexical_or_tsquery(""), "");
        assert_eq!(lexical_or_tsquery("the about from"), "");

        let query = lexical_or_tsquery("user's billing data");
        assert!(
            !query.contains("user's"),
            "apostrophes must be stripped: {query}"
        );
        assert_eq!(
            query.matches('\'').count() % 2,
            0,
            "every quote must be a balancing wrapper: {query}"
        );
        assert!(query.contains("'user'"), "expected quoted term: {query}");
        assert!(query.contains("'data'"), "expected quoted term: {query}");
    }

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

    #[test]
    fn lexical_fallback_terms_add_structured_memory_aliases() {
        // Pins: conversational first-person memory queries search structured
        // predicate fields even when name_tsv returns generic rows.
        let private_terms = lexical_fallback_terms(
            "Using exact memory id \"pr-s02-t00-u04-private-repository\", Which private work repository should you use for me?",
        );
        assert!(private_terms.contains(&"private_repository".to_string()));

        let preference_terms =
            lexical_fallback_terms("Format your next implementation answer the way I prefer.");
        assert!(preference_terms.contains(&"response_style".to_string()));

        assert!(should_run_lexical_fallback(true, &private_terms));
        assert!(should_run_lexical_fallback(true, &preference_terms));
        assert!(prefer_lexical_fallback_first(&private_terms));
        assert!(prefer_lexical_fallback_first(&preference_terms));
    }

    #[test]
    fn score_expansion_decays_activation_by_half_per_hop() {
        // Pins: a one-hop path outranks a two-hop path with the same seed strength and edge weight.
        let seed = uid(1);
        let near = uid(2);
        let far = uid(3);
        let ordered = score_expansion(
            &[
                expansion_hit(
                    seed,
                    far,
                    2,
                    vec![EdgeLabel::RelatesTo, EdgeLabel::RelatesTo],
                ),
                expansion_hit(seed, near, 1, vec![EdgeLabel::RelatesTo]),
            ],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert_eq!(ordered, vec![near, far]);
    }

    #[test]
    fn score_expansion_sums_paths_from_multiple_seeds() {
        // Pins: activation for the same target accumulates across independent seeds.
        let seed_a = uid(1);
        let seed_b = uid(2);
        let summed = uid(10);
        let single = uid(11);
        let ordered = score_expansion(
            &[
                expansion_hit(seed_a, single, 1, vec![EdgeLabel::RelatesTo]),
                expansion_hit(seed_a, summed, 1, vec![EdgeLabel::RelatesTo]),
                expansion_hit(seed_b, summed, 1, vec![EdgeLabel::RelatesTo]),
            ],
            &strengths(&[(seed_a, 1.0), (seed_b, 0.5)]),
            None,
            None,
        );

        assert_eq!(ordered[0], summed);
    }

    #[test]
    fn score_expansion_prefers_temporally_near_fact_for_equal_paths() {
        // Pins: multi-hop owner facts tied by path strength stay aligned with
        // the exact dependency seed instead of newer same-library facts.
        let seed = uid(1);
        let near = uid(2);
        let stale = uid(3);
        let seed_valid_from = chrono::Utc
            .with_ymd_and_hms(2026, 1, 10, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let ordered = score_expansion(
            &[
                expansion_hit_with_validity(
                    seed,
                    stale,
                    2,
                    vec![EdgeLabel::DependsOn, EdgeLabel::RelatesTo],
                    seed_valid_from,
                    seed_valid_from + chrono::Duration::days(70),
                ),
                expansion_hit_with_validity(
                    seed,
                    near,
                    2,
                    vec![EdgeLabel::DependsOn, EdgeLabel::RelatesTo],
                    seed_valid_from,
                    seed_valid_from + chrono::Duration::days(1),
                ),
            ],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert_eq!(ordered, vec![near, stale]);
    }

    #[test]
    fn score_expansion_zero_weight_edge_kills_path() {
        // Pins: contradiction paths never produce graph candidates.
        let seed = uid(1);
        let contradicted = uid(2);
        let ordered = score_expansion(
            &[expansion_hit(
                seed,
                contradicted,
                1,
                vec![EdgeLabel::Contradicts],
            )],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert!(ordered.is_empty());
    }

    #[test]
    fn score_expansion_prunes_supersedes_paths_without_as_of() {
        // Pins: present-time expansion does not leak superseded facts through SUPERSEDES.
        let seed = uid(1);
        let old = uid(2);
        let ordered = score_expansion(
            &[expansion_hit(seed, old, 1, vec![EdgeLabel::Supersedes])],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert!(ordered.is_empty());
    }

    #[test]
    fn score_expansion_allows_supersedes_paths_with_as_of() {
        // Pins: as-of expansion can traverse replacement -> superseded history.
        let seed = uid(1);
        let old = uid(2);
        let as_of = chrono::Utc
            .with_ymd_and_hms(2026, 3, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let ordered = score_expansion(
            &[expansion_hit(seed, old, 1, vec![EdgeLabel::Supersedes])],
            &strengths(&[(seed, 1.0)]),
            Some(as_of),
            None,
        );

        assert_eq!(ordered, vec![old]);
    }

    #[test]
    fn score_expansion_drops_entity_labeled_candidates() {
        // Pins: Entity rows are conduits by default and do not consume final candidate slots.
        let seed = uid(1);
        let entity = uid(2);
        let fact = uid(3);
        let mut entity_hit = expansion_hit(seed, entity, 1, vec![EdgeLabel::RelatesTo]);
        entity_hit.label = NodeLabel::Entity;
        let ordered = score_expansion(
            &[
                entity_hit,
                expansion_hit(
                    seed,
                    fact,
                    2,
                    vec![EdgeLabel::RelatesTo, EdgeLabel::RelatesTo],
                ),
            ],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert_eq!(ordered, vec![fact]);
    }

    #[tokio::test]
    async fn vector_leg_excludes_entity_labels_unless_explicitly_requested() {
        // Pins: vector retrieval follows the same Entity-conduit default as graph expansion.
        let observed = Arc::new(Mutex::new(Vec::new()));
        let vector = RecordingVectorStore {
            observed: Arc::clone(&observed),
        };
        let mut request = retrieval_request();

        vector_leg(&vector, &request)
            .await
            .expect("default vector leg should run");
        request.label_filter = Some(vec![NodeLabel::Entity]);
        vector_leg(&vector, &request)
            .await
            .expect("explicit entity vector leg should run");

        let observed = observed.lock().expect("observed label filters").clone();
        assert_eq!(observed.len(), 2);
        assert_eq!(
            observed[0],
            Some(vec![
                "Concept".to_string(),
                "Decision".to_string(),
                "Incident".to_string(),
                "Lesson".to_string(),
                "Fact".to_string(),
                "Source".to_string(),
            ])
        );
        assert_eq!(observed[1], Some(vec!["Entity".to_string()]));
    }

    #[test]
    fn score_expansion_orders_by_activation_then_uid() {
        // Pins: activation ties sort by uid for byte-stable reports.
        let seed = uid(1);
        let lower = uid(2);
        let higher = uid(3);
        let ordered = score_expansion(
            &[
                expansion_hit(seed, higher, 1, vec![EdgeLabel::RelatesTo]),
                expansion_hit(seed, lower, 1, vec![EdgeLabel::RelatesTo]),
            ],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert_eq!(ordered, vec![lower, higher]);
    }

    #[test]
    fn score_expansion_drops_seed_self_paths() {
        // Pins: undirected traversal cycles do not mark phase-one seeds as graph discoveries.
        let seed = uid(1);
        let ordered = score_expansion(
            &[expansion_hit(
                seed,
                seed,
                2,
                vec![EdgeLabel::RelatesTo, EdgeLabel::RelatesTo],
            )],
            &strengths(&[(seed, 1.0)]),
            None,
            None,
        );

        assert!(ordered.is_empty());
    }

    #[test]
    fn leg_sources_mark_graph_for_expansion_candidates() {
        // Pins: expansion candidates keep graph attribution through RRF fusion.
        let graph_uid = uid(1);
        let fused = rrf_fuse(
            &[LegCandidate {
                uid: graph_uid,
                score: 1.0 / 61.0,
            }],
            &[],
            &[],
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT),
        );

        assert_eq!(fused[0].0, graph_uid);
        assert!(fused[0].2.graph);
        assert!(!fused[0].2.vector);
        assert!(!fused[0].2.lexical);
    }

    #[test]
    fn exact_seed_candidates_emit_matching_fact_seed() {
        // Pins: exact non-Entity graph seeds can carry phase-one facts into graph attribution.
        let exact = uid(10);
        let sibling = uid(11);
        let ordered = exact_seed_candidates(
            &[
                node_row(exact, NodeLabel::Fact, "audit-shipper-dep-0-0-0"),
                node_row(sibling, NodeLabel::Fact, "audit-shipper-dep-0-4-0"),
            ],
            &strengths(&[(exact, 0.5), (sibling, 1.0)]),
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
            None,
        );

        assert_eq!(ordered, vec![exact]);
    }

    #[test]
    fn exact_seed_candidates_keep_entities_as_conduits() {
        // Pins: exact Entity seeds are not emitted unless the caller explicitly asks for Entity rows.
        let entity = uid(10);
        let ordered = exact_seed_candidates(
            &[node_row(
                entity,
                NodeLabel::Entity,
                "audit-shipper-dep-0-0-0",
            )],
            &strengths(&[(entity, 1.0)]),
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
            None,
        );

        assert!(ordered.is_empty());
    }

    #[test]
    fn merge_ordered_uids_preserves_first_occurrence() {
        // Pins: graph seed candidates precede expansion candidates without duplicate rows.
        let seed = uid(1);
        let expanded = uid(2);

        let ordered = merge_ordered_uids(vec![seed], vec![expanded, seed]);

        assert_eq!(ordered, vec![seed, expanded]);
    }

    fn uid(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn retrieval_request() -> RetrievalRequest {
        RetrievalRequest {
            seeds: Vec::new(),
            query_text: "who owns the dependency".to_string(),
            query_embedding: vec![0.1, 0.2],
            scope: MemoryScope::Tenant {
                tenant_id: TenantId::from(Uuid::from_u128(0x100)),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: 4,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: true,
            disable_graph_expansion: false,
        }
    }

    struct RecordingVectorStore {
        observed: Arc<Mutex<Vec<Option<Vec<String>>>>>,
    }

    #[async_trait]
    impl moa_memory_vector::VectorStore for RecordingVectorStore {
        fn backend(&self) -> &'static str {
            "recording"
        }

        fn dimension(&self) -> usize {
            2
        }

        async fn upsert(&self, _items: &[VectorItem]) -> Result<(), VectorError> {
            Ok(())
        }

        async fn upsert_in_tx(
            &self,
            _conn: &mut PgConnection,
            _items: &[VectorItem],
        ) -> Result<(), VectorError> {
            Ok(())
        }

        async fn knn(&self, query: &VectorQuery) -> Result<Vec<VectorMatch>, VectorError> {
            self.observed
                .lock()
                .expect("observed label filters")
                .push(query.label_filter.clone());
            Ok(Vec::new())
        }

        async fn delete(&self, _uids: &[Uuid]) -> Result<(), VectorError> {
            Ok(())
        }

        async fn delete_in_tx(
            &self,
            _conn: &mut PgConnection,
            _uids: &[Uuid],
        ) -> Result<(), VectorError> {
            Ok(())
        }
    }

    fn expansion_hit(seed: Uuid, uid: Uuid, hop: u8, edges: Vec<EdgeLabel>) -> GraphExpansionHit {
        let valid_from = chrono::Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        expansion_hit_with_validity(seed, uid, hop, edges, valid_from, valid_from)
    }

    fn expansion_hit_with_validity(
        seed: Uuid,
        uid: Uuid,
        hop: u8,
        edges: Vec<EdgeLabel>,
        seed_valid_from: chrono::DateTime<chrono::Utc>,
        valid_from: chrono::DateTime<chrono::Utc>,
    ) -> GraphExpansionHit {
        GraphExpansionHit {
            uid,
            label: NodeLabel::Fact,
            seed,
            seed_valid_from,
            valid_from,
            hop,
            edges,
        }
    }

    fn strengths(pairs: &[(Uuid, f64)]) -> HashMap<Uuid, f64> {
        pairs.iter().copied().collect()
    }

    fn node_row(uid: Uuid, label: NodeLabel, name: &str) -> NodeIndexRow {
        NodeIndexRow {
            uid,
            label,
            storage_partition_id: Some("tenant".to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: chrono::Utc::now(),
            properties_summary: None,
            last_accessed_at: chrono::Utc::now(),
            quality_score: 0.5,
        }
    }
}
