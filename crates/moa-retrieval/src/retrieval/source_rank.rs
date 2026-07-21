//! Source-object ranking and final context selection for tenant knowledge retrieval.

use std::collections::{HashMap, HashSet};

use moa_memory_graph::NodeLabel;
use moa_memory_types::MemoryScope;
use uuid::Uuid;

use crate::retrieval::policy::GraphRetrievalPolicy;
use crate::retrieval::ranking::normalize_tokens;
use crate::retrieval::types::{
    GraphPathTrace, GraphSeedSource, RetrievalHit, RetrievalRequest,
    SourceObjectFeatureContribution, SourceObjectFeatureContributions,
    SourceObjectRankingDiagnostics,
};

const MAX_FINAL_HITS_PER_KNOWLEDGE_OBJECT: usize = 2;
const SOURCE_GRAPH_DIAGNOSTIC_LIMIT: usize = 10;
const SOURCE_GRAPH_MAX_GRAPH_CONTRIBUTION: f64 = 0.09;

#[derive(Debug)]
struct SourceObjectAccumulator {
    object_uid: Uuid,
    source_uri: Option<String>,
    source_title: Option<String>,
    chunks: Vec<RetrievalHit>,
    rank_before_source_graph: usize,
    max_fused_score: f64,
    lexical_hit_count: usize,
    typed_graph_evidence_count: usize,
    structural_only_graph_count: usize,
    features: SourceObjectFeatureContributions,
    score: f64,
}

impl SourceObjectAccumulator {
    fn from_hit(
        hit: RetrievalHit,
        rank_before_source_graph: usize,
        object_uid: Uuid,
        source_uri: Option<String>,
        source_title: Option<String>,
    ) -> Self {
        Self {
            object_uid,
            source_uri,
            source_title,
            chunks: vec![hit],
            rank_before_source_graph,
            max_fused_score: f64::NEG_INFINITY,
            lexical_hit_count: 0,
            typed_graph_evidence_count: 0,
            structural_only_graph_count: 0,
            features: SourceObjectFeatureContributions::default(),
            score: 0.0,
        }
    }

    fn push(&mut self, hit: RetrievalHit, rank_before_source_graph: usize) {
        self.rank_before_source_graph = self.rank_before_source_graph.min(rank_before_source_graph);
        self.chunks.push(hit);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CandidateGraphEvidence {
    typed_paths: usize,
    structural_only_paths: usize,
}

/// Applies source-object ranking to hydrated tenant knowledge chunks.
pub(crate) fn apply_source_object_graph_ranking(
    hits: &mut Vec<RetrievalHit>,
    req: &RetrievalRequest,
    path_traces: &[GraphPathTrace],
    vector_rank_one: Option<Uuid>,
    graph_policy: GraphRetrievalPolicy,
) -> SourceObjectRankingDiagnostics {
    if !request_is_tenant_chunk_source_graph(req) || hits.is_empty() {
        return SourceObjectRankingDiagnostics::default();
    }

    let graph_evidence = graph_evidence_by_candidate(path_traces, graph_policy);
    let mut source_objects = HashMap::<Uuid, SourceObjectAccumulator>::new();
    let mut source_object_by_uid = HashMap::<Uuid, Uuid>::new();
    let mut passthrough = Vec::new();
    for (index, hit) in hits.drain(..).enumerate() {
        let rank = index + 1;
        let Some(chunk) = hit.knowledge_chunk.as_ref() else {
            passthrough.push((rank, hit));
            continue;
        };
        let object_uid = chunk.object_uid;
        let source_uri = chunk.source_uri.clone();
        let source_title = chunk.source_title.clone();
        source_object_by_uid.insert(hit.uid, object_uid);
        match source_objects.entry(object_uid) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push(hit, rank);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(SourceObjectAccumulator::from_hit(
                    hit,
                    rank,
                    object_uid,
                    source_uri,
                    source_title,
                ));
            }
        }
    }

    if source_objects.is_empty() {
        hits.extend(
            passthrough
                .into_iter()
                .map(|(_, passthrough_hit)| passthrough_hit),
        );
        return SourceObjectRankingDiagnostics::default();
    }

    let query_tokens = normalize_tokens(&req.query_text);
    let mut ranked_source_objects = source_objects.into_values().collect::<Vec<_>>();
    for source_object in &mut ranked_source_objects {
        score_source_object(source_object, &query_tokens, &graph_evidence);
    }
    let original_top_source_object = ranked_source_objects
        .iter()
        .min_by_key(|source_object| source_object.rank_before_source_graph)
        .map(|source_object| source_object.object_uid);
    ranked_source_objects.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                left.rank_before_source_graph
                    .cmp(&right.rank_before_source_graph)
            })
            .then_with(|| left.object_uid.cmp(&right.object_uid))
    });
    preserve_vector_source_object_rank_one(
        &mut ranked_source_objects,
        vector_rank_one,
        &source_object_by_uid,
        graph_policy,
    );
    if graph_policy.uses_source_object_ranking()
        && ranked_source_objects.first().is_some_and(|source_object| {
            Some(source_object.object_uid) == original_top_source_object
        })
    {
        // When source-level scoring cannot change the top source object, lower-rank
        // reshuffling is more likely to add noise than useful rank lift.
        ranked_source_objects.sort_by(|left, right| {
            left.rank_before_source_graph
                .cmp(&right.rank_before_source_graph)
                .then_with(|| left.object_uid.cmp(&right.object_uid))
        });
    }

    let mut diagnostics = SourceObjectRankingDiagnostics {
        enabled: true,
        ranked_source_object_count: ranked_source_objects.len(),
        feature_totals: SourceObjectFeatureContributions::default(),
        top_source_objects: Vec::new(),
    };
    let mut ordered_hits = Vec::new();
    for (index, mut source_object) in ranked_source_objects.into_iter().enumerate() {
        let source_object_rank = index + 1;
        diagnostics.feature_totals.add(source_object.features);
        if diagnostics.top_source_objects.len() < SOURCE_GRAPH_DIAGNOSTIC_LIMIT {
            diagnostics
                .top_source_objects
                .push(source_object_feature_contribution(
                    &source_object,
                    source_object_rank,
                ));
        }
        order_source_object_chunks(&mut source_object);
        ordered_hits.extend(source_object.chunks);
    }
    passthrough.sort_by_key(|(rank, _)| *rank);
    ordered_hits.extend(
        passthrough
            .into_iter()
            .map(|(_, passthrough_hit)| passthrough_hit),
    );
    *hits = ordered_hits;
    diagnostics
}

fn request_is_tenant_chunk_source_graph(req: &RetrievalRequest) -> bool {
    matches!(req.scope, MemoryScope::Tenant { .. })
        && req
            .label_filter
            .as_deref()
            .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn score_source_object(
    source_object: &mut SourceObjectAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
    graph_evidence: &HashMap<Uuid, CandidateGraphEvidence>,
) {
    source_object.max_fused_score = source_object
        .chunks
        .iter()
        .map(|hit| hit.score)
        .fold(f64::NEG_INFINITY, f64::max);
    source_object.lexical_hit_count = source_object
        .chunks
        .iter()
        .filter(|hit| hit.legs.lexical)
        .count();
    source_object.typed_graph_evidence_count = source_object
        .chunks
        .iter()
        .filter_map(|hit| graph_evidence.get(&hit.uid))
        .map(|evidence| evidence.typed_paths)
        .sum();
    source_object.structural_only_graph_count = source_object
        .chunks
        .iter()
        .filter_map(|hit| graph_evidence.get(&hit.uid))
        .map(|evidence| evidence.structural_only_paths)
        .sum();
    source_object.features = source_object_feature_contributions(source_object, query_tokens);
    source_object.score = source_object.features.total();
}

fn source_object_feature_contributions(
    source_object: &SourceObjectAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
) -> SourceObjectFeatureContributions {
    let typed_graph_count = source_object.typed_graph_evidence_count as f64;
    let title_overlap = source_title_overlap(source_object, query_tokens);
    let lexical_bonus = if source_object.lexical_hit_count > 0 {
        0.025
    } else {
        0.0
    };
    let exact_title_match = if title_overlap >= 0.999 { 0.04 } else { 0.0 };
    let structural_penalty = if source_object.structural_only_graph_count > 0
        && source_object.typed_graph_evidence_count == 0
    {
        -0.04 * source_object.structural_only_graph_count.min(3) as f64
    } else {
        0.0
    };
    SourceObjectFeatureContributions {
        max_fused_score: source_object.max_fused_score,
        lexical_title: lexical_bonus + 0.05 * title_overlap,
        same_source_object_repeat: 0.0,
        exact_title_match,
        typed_graph_evidence: (0.03 * typed_graph_count).min(SOURCE_GRAPH_MAX_GRAPH_CONTRIBUTION),
        adjacent_chunk_support: 0.0,
        structural_only_penalty: structural_penalty,
    }
}

fn source_title_overlap(
    source_object: &SourceObjectAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let mut title_tokens = source_object
        .source_title
        .as_deref()
        .map(normalize_tokens)
        .unwrap_or_default();
    for chunk in source_object
        .chunks
        .iter()
        .filter_map(|hit| hit.knowledge_chunk.as_ref())
    {
        for heading in &chunk.heading_path {
            title_tokens.extend(normalize_tokens(heading));
        }
    }
    if title_tokens.is_empty() {
        return 0.0;
    }
    let overlap = title_tokens
        .iter()
        .filter(|token| query_tokens.contains(*token))
        .count();
    overlap as f64 / title_tokens.len() as f64
}

fn graph_evidence_by_candidate(
    path_traces: &[GraphPathTrace],
    graph_policy: GraphRetrievalPolicy,
) -> HashMap<Uuid, CandidateGraphEvidence> {
    let mut evidence = HashMap::<Uuid, CandidateGraphEvidence>::new();
    for trace in path_traces {
        if !graph_policy_allows_source_evidence_path(graph_policy, trace) {
            continue;
        }
        let entry = evidence.entry(trace.candidate_uid).or_default();
        if graph_path_is_structural_only(&trace.edge_labels) {
            entry.structural_only_paths += 1;
        } else {
            entry.typed_paths += 1;
        }
    }
    evidence
}

fn graph_policy_allows_source_evidence_path(
    graph_policy: GraphRetrievalPolicy,
    trace: &GraphPathTrace,
) -> bool {
    if graph_policy != GraphRetrievalPolicy::EntityLocalSearch {
        return true;
    }
    if trace.seed_source == Some(GraphSeedSource::BroadFallback) {
        return false;
    }
    if trace.hop == 1 && trace.edge_labels.len() == 1 && trace.edge_directions.len() == 1 {
        return trace.edge_labels[0] == "MENTIONED_IN" && trace.edge_directions[0] == "incoming";
    }
    trace.hop == 2
        && trace.edge_labels.len() == 2
        && trace.edge_directions.len() == 2
        && entity_local_source_evidence_semantic_step(
            &trace.edge_labels[0],
            &trace.edge_directions[0],
        )
        && trace.edge_labels[1] == "MENTIONED_IN"
        && trace.edge_directions[1] == "incoming"
}

fn entity_local_source_evidence_semantic_step(edge_label: &str, edge_direction: &str) -> bool {
    edge_direction == "outgoing"
        && matches!(
            edge_label,
            "RELATES_TO" | "DEPENDS_ON" | "OWNED_BY" | "CAUSED" | "LEARNED_FROM" | "APPLIES_TO"
        )
}

fn graph_path_is_structural_only(edge_labels: &[String]) -> bool {
    !edge_labels.is_empty()
        && edge_labels.iter().all(|label| {
            matches!(
                label.as_str(),
                "CONTAINS" | "HAS_DOCUMENT" | "HAS_CHUNK" | "contains"
            )
        })
}

fn preserve_vector_source_object_rank_one(
    ranked_source_objects: &mut [SourceObjectAccumulator],
    vector_rank_one: Option<Uuid>,
    source_object_by_uid: &HashMap<Uuid, Uuid>,
    graph_policy: GraphRetrievalPolicy,
) {
    let Some(vector_rank_one) = vector_rank_one else {
        return;
    };
    let Some(vector_source_object) = source_object_by_uid.get(&vector_rank_one).copied() else {
        return;
    };
    let Some(top_source_object) = ranked_source_objects.first() else {
        return;
    };
    if top_source_object.object_uid == vector_source_object {
        return;
    }
    if graph_policy == GraphRetrievalPolicy::SourceGraph
        && top_source_object.typed_graph_evidence_count > 0
    {
        return;
    }
    let Some(vector_index) = ranked_source_objects
        .iter()
        .position(|source_object| source_object.object_uid == vector_source_object)
    else {
        return;
    };
    ranked_source_objects[..=vector_index].rotate_right(1);
}

fn source_object_feature_contribution(
    source_object: &SourceObjectAccumulator,
    rank_after_source_graph: usize,
) -> SourceObjectFeatureContribution {
    SourceObjectFeatureContribution {
        object_uid: source_object.object_uid,
        source_uri: source_object.source_uri.clone(),
        source_title: source_object.source_title.clone(),
        chunk_count: source_object.chunks.len(),
        rank_before_source_graph: Some(source_object.rank_before_source_graph),
        rank_after_source_graph,
        rank_delta_after_minus_before: Some(
            rank_after_source_graph as i64 - source_object.rank_before_source_graph as i64,
        ),
        score: source_object.score,
        features: source_object.features,
        typed_graph_evidence_count: source_object.typed_graph_evidence_count,
        structural_only_graph_count: source_object.structural_only_graph_count,
    }
}

fn order_source_object_chunks(source_object: &mut SourceObjectAccumulator) {
    let best_ordinal = source_object
        .chunks
        .iter()
        .filter_map(|hit| {
            hit.knowledge_chunk
                .as_ref()
                .map(|chunk| (hit.score, chunk.ordinal))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, ordinal)| ordinal)
        .unwrap_or_default();
    source_object.chunks.sort_by(|left, right| {
        source_object_chunk_order_key(left, best_ordinal)
            .cmp(&source_object_chunk_order_key(right, best_ordinal))
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.uid.cmp(&right.uid))
    });
}

fn source_object_chunk_order_key(hit: &RetrievalHit, best_ordinal: i32) -> (u8, i32) {
    let Some(chunk) = &hit.knowledge_chunk else {
        return (2, i32::MAX);
    };
    let distance = (chunk.ordinal - best_ordinal).abs();
    if distance == 0 {
        (0, 0)
    } else if distance == 1 {
        (1, distance)
    } else {
        (2, distance)
    }
}

fn select_final_hits(
    primary: Vec<RetrievalHit>,
    fallback: &[RetrievalHit],
    k_final: usize,
) -> Vec<RetrievalHit> {
    let mut selected = Vec::with_capacity(k_final);
    let mut selected_uids = HashSet::new();
    let mut object_counts = HashMap::<Uuid, usize>::new();
    let mut selected_facets = HashSet::new();
    let mut facet_duplicates = Vec::new();
    for hit in primary {
        // Facts restating identical content (same subject/predicate/object)
        // waste final-context slots on information an earlier hit already
        // carries; defer them so a later distinct fact (typically the second
        // hop of a multi-hop question) can take the slot. Update-era facts
        // keep distinct objects, so bitemporal families are never collapsed.
        // A facet counts as selected only when its hit actually survives the
        // uid and per-object caps; a rejected hit must not block a later
        // representative of the same content.
        let facet = fact_content_facet(&hit);
        if let Some(facet) = &facet
            && selected_facets.contains(facet)
        {
            facet_duplicates.push(hit);
            continue;
        }
        if push_capped_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        ) && let Some(facet) = facet
        {
            selected_facets.insert(facet);
        }
        if selected.len() == k_final {
            return selected;
        }
    }
    for hit in fallback {
        let facet = fact_content_facet(hit);
        if let Some(facet) = &facet
            && selected_facets.contains(facet)
        {
            facet_duplicates.push(hit.clone());
            continue;
        }
        if push_capped_hit(
            hit.clone(),
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        ) && let Some(facet) = facet
        {
            selected_facets.insert(facet);
        }
        if selected.len() == k_final {
            return selected;
        }
    }
    // Fewer distinct facets than k: backfill with the deferred duplicates in
    // their original fused order so the cutoff never returns fewer hits than
    // the undiversified selection would have.
    for hit in facet_duplicates {
        push_capped_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        );
        if selected.len() == k_final {
            break;
        }
    }
    selected
}

/// Returns the normalized `(subject, predicate, object)` content facet for a
/// `Fact` hit, or `None` for non-fact hits or facts missing any component.
fn fact_content_facet(hit: &RetrievalHit) -> Option<(String, String, String)> {
    if hit.node.label != NodeLabel::Fact {
        return None;
    }
    let properties = hit.node.properties_summary.as_ref()?;
    let component = |key: &str| {
        properties
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(moa_memory_types::normalize_fact_component)
            .filter(|value| !value.is_empty())
    };
    Some((
        component("subject")?,
        component("predicate")?,
        component("object")?,
    ))
}

/// Selects final hits using source diversity for source-object graph policies.
pub(crate) fn select_final_hits_for_policy(
    primary: Vec<RetrievalHit>,
    fallback: &[RetrievalHit],
    k_final: usize,
    graph_policy: GraphRetrievalPolicy,
) -> Vec<RetrievalHit> {
    if graph_policy.uses_source_object_ranking() {
        return select_source_diverse_context_hits(primary, fallback, k_final);
    }
    select_final_hits(primary, fallback, k_final)
}

fn select_source_diverse_context_hits(
    primary: Vec<RetrievalHit>,
    fallback: &[RetrievalHit],
    k_final: usize,
) -> Vec<RetrievalHit> {
    let mut selected = Vec::with_capacity(k_final);
    let mut selected_uids = HashSet::new();
    let mut object_counts = HashMap::<Uuid, usize>::new();
    let mut primary_support = Vec::new();
    for hit in primary {
        push_source_diverse_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            &mut primary_support,
            k_final,
        );
        if selected.len() == k_final {
            return selected;
        }
    }

    let mut fallback_support = Vec::new();
    for hit in fallback {
        push_source_diverse_hit(
            hit.clone(),
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            &mut fallback_support,
            k_final,
        );
        if selected.len() == k_final {
            return selected;
        }
    }

    for hit in primary_support {
        push_capped_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        );
        if selected.len() == k_final {
            return selected;
        }
    }
    for hit in fallback_support {
        push_capped_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        );
        if selected.len() == k_final {
            break;
        }
    }
    selected
}

fn push_source_diverse_hit(
    hit: RetrievalHit,
    selected: &mut Vec<RetrievalHit>,
    selected_uids: &mut HashSet<Uuid>,
    object_counts: &mut HashMap<Uuid, usize>,
    support: &mut Vec<RetrievalHit>,
    k_final: usize,
) {
    if selected.len() == k_final || selected_uids.contains(&hit.uid) {
        return;
    }
    let Some(object_uid) = hit.knowledge_chunk.as_ref().map(|chunk| chunk.object_uid) else {
        selected_uids.insert(hit.uid);
        selected.push(hit);
        return;
    };
    if object_counts.contains_key(&object_uid) {
        support.push(hit);
        return;
    }
    selected_uids.insert(hit.uid);
    object_counts.insert(object_uid, 1);
    selected.push(hit);
}

/// Pushes a hit unless it violates the uid or per-object caps; returns whether
/// the hit was actually selected.
fn push_capped_hit(
    hit: RetrievalHit,
    selected: &mut Vec<RetrievalHit>,
    selected_uids: &mut HashSet<Uuid>,
    object_counts: &mut HashMap<Uuid, usize>,
    k_final: usize,
) -> bool {
    if selected.len() == k_final || !selected_uids.insert(hit.uid) {
        return false;
    }
    if let Some(object_uid) = hit.knowledge_chunk.as_ref().map(|chunk| chunk.object_uid) {
        let count = object_counts.entry(object_uid).or_default();
        if *count >= MAX_FINAL_HITS_PER_KNOWLEDGE_OBJECT {
            selected_uids.remove(&hit.uid);
            return false;
        }
        *count += 1;
    }
    selected.push(hit);
    true
}
