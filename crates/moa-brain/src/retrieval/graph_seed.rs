//! Graph seed planning for hybrid retrieval.

use std::collections::{HashMap, HashSet};

use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass, push_validity_filter};
use moa_memory_types::MemoryScope;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::retrieval::legs::{begin_scoped, hydrate_nodes};
use crate::retrieval::policy::GraphRetrievalPolicy;
use crate::retrieval::ranking::normalize_tokens;
use crate::retrieval::types::{
    GraphSeedDiagnostics, GraphSeedSource, LegSources, Result, RetrievalRequest,
};

const PHASE_ONE_GRAPH_SEED_LIMIT: usize = 26;
const PHASE_ONE_SEED_DECAY: f64 = 0.85;
const SEMANTIC_ENTITY_GRAPH_SEED_LIMIT: usize = 8;
const SEMANTIC_ENTITY_EXACT_SEED_LIMIT: usize = 3;
const SEMANTIC_ENTITY_SEED_STRENGTH: f64 = 0.90;

/// Planned graph seeds plus diagnostics for one retrieval request.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct GraphSeedPlan {
    /// Weighted graph traversal seed list.
    pub(crate) strengths: Vec<(Uuid, f64)>,
    /// Seed counts grouped by admission source.
    pub(crate) seed_counts: GraphSeedDiagnostics,
    /// Admission source for each seed uid.
    pub(crate) seed_sources: HashMap<Uuid, GraphSeedSource>,
}

/// Builds the graph seed plan from planner, semantic entity, and phase-one hits.
pub(crate) fn interim_graph_seed_plan(
    policy: GraphRetrievalPolicy,
    planner_seeds: &[Uuid],
    semantic_entity_seed_uids: &[Uuid],
    interim: &[(Uuid, f64, LegSources)],
    phase_one_rows: &[NodeIndexRow],
    query_text: &str,
) -> GraphSeedPlan {
    let mut seen = HashSet::new();
    let mut strengths = Vec::new();
    let mut seed_counts = GraphSeedDiagnostics::default();
    let mut seed_sources = HashMap::new();
    for seed in planner_seeds {
        if seen.insert(*seed) {
            strengths.push((*seed, 1.0));
            seed_counts.planner += 1;
            seed_sources.insert(*seed, GraphSeedSource::Planner);
        }
    }
    for seed in
        exact_semantic_entity_seed_uids(semantic_entity_seed_uids, phase_one_rows, query_text)
    {
        if seen.insert(seed) {
            strengths.push((seed, SEMANTIC_ENTITY_SEED_STRENGTH));
            seed_counts.semantic_entity += 1;
            seed_sources.insert(seed, GraphSeedSource::SemanticEntity);
        }
    }

    let exact_seed_uids = exact_phase_one_seed_uids(phase_one_rows, query_text);
    let use_broad_phase_one = policy.allows_broad_phase_one_fallback()
        && planner_seeds.is_empty()
        && exact_seed_uids.is_empty();
    let mut phase_one = interim
        .iter()
        .take(PHASE_ONE_GRAPH_SEED_LIMIT)
        .enumerate()
        .filter_map(|(index, (uid, _, _))| {
            let is_exact = exact_seed_uids.contains(uid);
            (is_exact || use_broad_phase_one).then_some((*uid, index, is_exact))
        })
        .collect::<Vec<_>>();
    phase_one.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.1.cmp(&right.1)));
    for (index, (uid, _, is_exact)) in phase_one.into_iter().enumerate() {
        if seen.insert(uid) {
            strengths.push((uid, PHASE_ONE_SEED_DECAY.powi(index as i32)));
            if is_exact {
                seed_counts.exact_phase_one += 1;
                seed_sources.insert(uid, GraphSeedSource::ExactPhaseOne);
            } else {
                seed_counts.broad_fallback += 1;
                seed_sources.insert(uid, GraphSeedSource::BroadFallback);
            }
        }
    }
    GraphSeedPlan {
        strengths,
        seed_counts,
        seed_sources,
    }
}

/// Hydrates graph seed rows required for exact seed gating.
pub(crate) async fn hydrate_graph_seed_rows(
    pool: &PgPool,
    req: &RetrievalRequest,
    interim: &[(Uuid, f64, LegSources)],
    semantic_entity_seed_uids: &[Uuid],
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
        semantic_entity_seed_uids
            .iter()
            .copied()
            .filter(|uid| seen.insert(*uid)),
    );
    uids.extend(
        interim
            .iter()
            .take(PHASE_ONE_GRAPH_SEED_LIMIT)
            .map(|(uid, _, _)| *uid)
            .filter(|uid| seen.insert(*uid)),
    );
    hydrate_nodes(pool, &req.scope, &uids, assume_app_role, req.as_of).await
}

/// Looks up exact semantic entity graph seeds for explicit slow policies.
pub(crate) async fn semantic_entity_seed_uids(
    pool: &PgPool,
    req: &RetrievalRequest,
    assume_app_role: bool,
) -> Result<Vec<Uuid>> {
    if !request_is_tenant_chunk_query(req) {
        return Ok(Vec::new());
    }
    let terms = semantic_entity_seed_terms(&req.query_text);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let tsquery = terms
        .iter()
        .map(|term| format!("'{}':*", term.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(" | ");
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
    builder.push(" AND scope = 'tenant' AND label = ");
    builder.push_bind(NodeLabel::Entity.as_str());
    builder.push(r#" AND (name_tsv @@ to_tsquery('simple', "#);
    builder.push_bind(tsquery.clone());
    builder.push(") OR properties_tsv @@ to_tsquery('simple', ");
    builder.push_bind(tsquery.clone());
    builder.push(
        r#"))
          AND CASE pii_class
                WHEN 'none' THEN 0
                WHEN 'pii' THEN 1
                WHEN 'phi' THEN 2
                WHEN 'restricted' THEN 3
                ELSE 4
              END <= "#,
    );
    builder.push_bind(semantic_seed_pii_rank(req.max_pii_class));
    builder.push(
        r#"
        ORDER BY (
            ts_rank(name_tsv, to_tsquery('simple', "#,
    );
    builder.push_bind(tsquery.clone());
    builder.push(")) + ts_rank(properties_tsv, to_tsquery('simple', ");
    builder.push_bind(tsquery);
    builder.push(
        r#"))
        ) DESC,
        uid ASC
        LIMIT "#,
    );
    builder.push_bind(SEMANTIC_ENTITY_GRAPH_SEED_LIMIT as i64);
    let rows = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(conn.as_mut())
        .await?;
    conn.commit().await?;
    Ok(rows)
}

fn request_is_tenant_chunk_query(req: &RetrievalRequest) -> bool {
    matches!(req.scope, MemoryScope::Tenant { .. })
        && req
            .label_filter
            .as_deref()
            .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn semantic_entity_seed_terms(query_text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "also", "before", "between", "could", "does", "from", "have", "help",
        "into", "make", "need", "should", "that", "their", "there", "these", "this", "using",
        "what", "when", "where", "which", "with", "your",
    ];
    normalize_tokens(query_text)
        .into_iter()
        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
        .take(SEMANTIC_ENTITY_GRAPH_SEED_LIMIT)
        .collect()
}

const fn semantic_seed_pii_rank(class: PiiClass) -> i32 {
    match class {
        PiiClass::None => 0,
        PiiClass::Pii => 1,
        PiiClass::Phi => 2,
        PiiClass::Restricted => 3,
    }
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

fn exact_semantic_entity_seed_uids(
    semantic_entity_seed_uids: &[Uuid],
    rows: &[NodeIndexRow],
    query_text: &str,
) -> Vec<Uuid> {
    let query_tokens = normalize_tokens(query_text);
    let rows_by_uid = rows
        .iter()
        .map(|row| (row.uid, row))
        .collect::<HashMap<_, _>>();
    semantic_entity_seed_uids
        .iter()
        .filter_map(|uid| rows_by_uid.get(uid).copied())
        .filter(|row| row.label == NodeLabel::Entity)
        .filter(|row| {
            let name_tokens = normalize_tokens(&row.name);
            name_tokens.len() >= 2 && name_tokens.iter().all(|token| query_tokens.contains(token))
        })
        .map(|row| row.uid)
        .take(SEMANTIC_ENTITY_EXACT_SEED_LIMIT)
        .collect()
}

#[cfg(test)]
fn interim_graph_seed_strengths(
    planner_seeds: &[Uuid],
    interim: &[(Uuid, f64, LegSources)],
    phase_one_rows: &[NodeIndexRow],
    query_text: &str,
) -> Vec<(Uuid, f64)> {
    interim_graph_seed_plan(
        GraphRetrievalPolicy::LegacyBroadExpansion,
        planner_seeds,
        &[],
        interim,
        phase_one_rows,
        query_text,
    )
    .strengths
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn anchored_rescue_seed_selection_suppresses_broad_phase_one_fallback() {
        // Pins: AnchoredRescue does not seed graph traversal from generic top
        // vector or lexical candidates when there is no exact anchor.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::AnchoredRescue,
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert!(plan.strengths.is_empty());
        assert_eq!(plan.seed_counts, GraphSeedDiagnostics::default());
    }

    #[test]
    fn entity_local_search_seed_selection_accepts_semantic_entity_seeds() {
        // Pins: semantic entity anchors can start graph traversal without
        // re-enabling generic broad phase-one fallback.
        let semantic_seed = Uuid::from_u128(42);
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();
        let mut semantic_row = node_row(semantic_seed, "custom domain");
        semantic_row.label = NodeLabel::Entity;

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::EntityLocalSearch,
            &[],
            &[semantic_seed],
            &interim,
            &[semantic_row],
            "custom domain",
        );

        assert_eq!(
            plan.strengths,
            vec![(semantic_seed, SEMANTIC_ENTITY_SEED_STRENGTH)]
        );
        assert_eq!(plan.seed_counts.semantic_entity, 1);
        assert_eq!(plan.seed_counts.broad_fallback, 0);
        assert_eq!(
            plan.seed_sources.get(&semantic_seed),
            Some(&GraphSeedSource::SemanticEntity)
        );
    }

    #[test]
    fn interim_seed_selection_keeps_planner_strength_without_broad_phase_one_fallback() {
        // Pins: planner NER seeds keep strength 1.0 and suppress broad phase-one fallback.
        let collision = Uuid::from_u128(1);
        let interim = (1_u128..=(PHASE_ONE_GRAPH_SEED_LIMIT as u128 + 4))
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let strengths = interim_graph_seed_strengths(&[collision], &interim, &[], "");

        assert_eq!(strengths, vec![(collision, 1.0)]);
    }

    #[test]
    fn graph_seed_selection_caps_broad_phase_one_when_planner_and_exact_seeds_empty() {
        // Pins: graph expansion can still run when NER finds no planner seeds.
        let interim = (1_u128..=(PHASE_ONE_GRAPH_SEED_LIMIT as u128 + 4))
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let strengths = interim_graph_seed_strengths(&[], &interim, &[], "");

        assert_eq!(strengths.len(), PHASE_ONE_GRAPH_SEED_LIMIT);
        assert_eq!(strengths[0], (Uuid::from_u128(1), 1.0));
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
    }

    #[test]
    fn default_graph_policy_suppresses_broad_phase_one_fallback() {
        // Pins: the production default uses AnchoredRescue guardrails and does
        // not seed graph traversal from generic phase-one vector/lexical hits.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::default(),
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert_eq!(
            GraphRetrievalPolicy::default(),
            GraphRetrievalPolicy::AnchoredRescue
        );
        assert!(plan.strengths.is_empty());
        assert_eq!(plan.seed_counts, GraphSeedDiagnostics::default());
    }

    #[test]
    fn explicit_legacy_graph_policy_preserves_broad_phase_one_fallback() {
        // Pins: the legacy A/B policy still exposes the old broad fallback
        // behavior without making it the default.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::LegacyBroadExpansion,
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert_eq!(
            plan.strengths,
            vec![
                (Uuid::from_u128(1), 1.0),
                (Uuid::from_u128(2), PHASE_ONE_SEED_DECAY),
                (Uuid::from_u128(3), PHASE_ONE_SEED_DECAY.powi(2)),
            ]
        );
        assert_eq!(
            plan.seed_counts,
            GraphSeedDiagnostics {
                planner: 0,
                exact_phase_one: 0,
                broad_fallback: 3,
                semantic_entity: 0,
            }
        );
    }

    #[test]
    fn interim_seed_selection_uses_exact_phase_one_subject_match_without_broad_siblings() {
        // Pins: exact entity mentions seed graph expansion without same-shape siblings.
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

        assert_eq!(strengths, vec![(exact, 1.0)]);
    }

    #[test]
    fn interim_seed_selection_keeps_planner_first_and_exact_phase_one_only() {
        // Pins: planner seeds stay first while exact phase-one subjects pass through alone.
        let planner = Uuid::from_u128(9);
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
            &[planner],
            &interim,
            &rows,
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
        );

        assert_eq!(strengths, vec![(planner, 1.0), (exact, 1.0)]);
    }

    fn node_row(uid: Uuid, name: &str) -> NodeIndexRow {
        NodeIndexRow {
            uid,
            label: NodeLabel::Fact,
            storage_partition_id: Some("tenant".to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: Utc::now(),
            properties_summary: None,
            last_accessed_at: Utc::now(),
            quality_score: 0.5,
        }
    }
}
