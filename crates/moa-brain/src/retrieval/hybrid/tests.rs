//! Unit tests for hybrid retrieval coordination and ranking behavior.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;

#[test]
fn lineage_sampling_is_deterministic_and_respects_rate_bounds() {
    // Pins: lineage sampling keys on (session, turn) so the same turn always
    // makes the same decision, 1.0 records everything, 0.0 records nothing,
    // and a partial rate keeps roughly that fraction of turns.
    let session_id = moa_core::types::identifiers::SessionId(uuid::Uuid::from_u128(0x5eed));
    let lineage = |turn_seq: i64| LineageContext {
        session_id,
        turn_id: None,
        turn_seq,
    };

    for turn_seq in 0..64 {
        assert!(lineage_turn_sampled(&lineage(turn_seq), 1.0));
        assert!(!lineage_turn_sampled(&lineage(turn_seq), 0.0));
        assert_eq!(
            lineage_turn_sampled(&lineage(turn_seq), 0.5),
            lineage_turn_sampled(&lineage(turn_seq), 0.5),
            "sampling must be deterministic per (session, turn)"
        );
    }

    let sampled = (0..1_000)
        .filter(|turn_seq| lineage_turn_sampled(&lineage(*turn_seq), 0.5))
        .count();
    assert!(
        (350..=650).contains(&sampled),
        "a 0.5 rate should keep roughly half of 1000 turns, kept {sampled}"
    );
}

use moa_core::types::security::SensitivityClass;
use moa_core::{types::identifiers::TenantId, types::memory::RlsContext};
use moa_memory_graph::GraphError;
use moa_memory_types::MemoryScope;
use moa_providers::{RerankHit, Reranker};
use serde_json::Value;
use uuid::Uuid;

use super::*;
use crate::retrieval::types::{GraphPathTrace, GraphSeedSource, KnowledgeChunkHydration};

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
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: "deploy provider".to_string(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 1,
        use_reranker: true,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
    };
    let first = hit(Uuid::now_v7(), "workspace", 2.0);
    let second = hit(Uuid::now_v7(), "workspace", 1.0);

    let reranked = retriever
        .rerank_hits(&req, &[first.clone(), second.clone()])
        .await
        .expect("rerank should succeed");

    assert_eq!(reranked.hits, vec![second.clone()]);
    // Pins: a successful rerank captures the per-candidate score keyed by the
    // reranked hit's uid and its pre-rerank index, so retrieval lineage records
    // the real reranker output instead of fabricating it from fused order.
    assert_eq!(reranked.scores.len(), 1);
    assert_eq!(reranked.scores[0].uid, second.uid);
    assert_eq!(reranked.scores[0].original_index, 1);
    assert!((reranked.scores[0].relevance_score - 1.0).abs() < f32::EPSILON);
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
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: "how do I connect a custom domain?".to_string(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 1,
        use_reranker: true,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "contact email".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
        },
    );

    assert_eq!(hits[0].uid, lexical_uid);
}

#[test]
fn label_boost_reorders_without_excluding_non_hinted() {
    // Pins: a planner-inferred label hint (`label_boost`) lifts a hinted-label
    // candidate over an otherwise-equal non-hinted candidate, yet the
    // non-hinted candidate is still retained and ranked. The non-hinted hit is
    // given the smaller uid so it wins the deterministic tie-break; only the
    // boost can move the hinted hit ahead of it.
    let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .expect("test timestamp should parse")
        .with_timezone(&Utc);
    let non_hinted_uid = Uuid::from_u128(1);
    let hinted_uid = Uuid::from_u128(2);

    let mut non_hinted = hit(non_hinted_uid, "tenant", 1.0);
    non_hinted.node.label = NodeLabel::Fact;
    non_hinted.node.name = "auth outage record".to_string();
    non_hinted.node.valid_from = reference_time;
    non_hinted.node.last_accessed_at = reference_time;
    let mut hinted = hit(hinted_uid, "tenant", 1.0);
    hinted.node.label = NodeLabel::Lesson;
    hinted.node.name = "auth outage record".to_string();
    hinted.node.valid_from = reference_time;
    hinted.node.last_accessed_at = reference_time;
    let mut hits = vec![non_hinted, hinted];

    rank_hydrated_hits(
        &mut hits,
        &RankingConfig::default(),
        &RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "what did we learn from the auth outage?".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            label_boost: Some(vec![NodeLabel::Lesson]),
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
        },
    );

    assert_eq!(hits.len(), 2, "non-hinted candidate must not be excluded");
    assert_eq!(hits[0].uid, hinted_uid, "hinted label must rank first");
    assert_eq!(hits[1].uid, non_hinted_uid);
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "regional network".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "library owner".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "library owner".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 3,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 3,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 3,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "custom domain dns records".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 2,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
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

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::SourceGraph);

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

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::SourceGraph);

    assert_eq!(
        selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        vec![a1_uid, b1_uid, a2_uid]
    );
}

#[test]
fn non_source_graph_selection_keeps_existing_support_order() {
    // Pins: source-diverse selection does not change non-source-graph
    // final-hit ordering.
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

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::AnchoredRescue);

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
fn empty_fusion_retries_vector_only_after_an_observed_timeout() {
    // Pins: F09 — a timed-out vector leg can mask candidates for an embedded
    // query, so exactly one bounded retry is allowed. A genuinely empty (or
    // transiently degraded) vector leg is NOT retried, avoiding duplicate work.
    let req = vector_request();

    assert!(should_retry_vector_after_empty_fusion(&req, true, &[], &[]));
    assert!(!should_retry_vector_after_empty_fusion(
        &req,
        false,
        &[],
        &[]
    ));
}

#[test]
fn empty_fusion_vector_retry_stays_off_when_a_peer_leg_has_candidates() {
    // Pins: the timeout retry is only for complete candidate loss, not when
    // lexical or graph already produced candidates.
    let req = vector_request();
    let candidate = leg_candidate(Uuid::from_u128(1));

    assert!(!should_retry_vector_after_empty_fusion(
        &req,
        true,
        &[candidate],
        &[]
    ));
    assert!(!should_retry_vector_after_empty_fusion(
        &req,
        true,
        &[],
        &[candidate]
    ));
}

#[test]
fn empty_fusion_vector_retry_respects_timeout_override() {
    // Pins: callers that disabled leg timeouts asked for uncapped execution,
    // so there is no bounded timeout to retry.
    let mut req = vector_request();
    req.disable_leg_timeouts = true;

    assert!(!should_retry_vector_after_empty_fusion(
        &req,
        true,
        &[],
        &[]
    ));
}

#[tokio::test]
async fn run_leg_classifies_success_transient_fatal_and_timeout() {
    // Pins: F09 — run_leg threads the reason a leg produced no hits instead of
    // collapsing everything to an empty default.
    let success = run_leg::<Vec<LegCandidate>, _>(false, "vector", VECTOR_BUDGET, async {
        Ok(vec![leg_candidate(Uuid::from_u128(1))])
    })
    .await;
    assert!(matches!(success, LegOutcome::Completed(ref hits) if hits.len() == 1));

    let transient = run_leg::<Vec<LegCandidate>, _>(false, "vector", VECTOR_BUDGET, async {
        Err(RetrievalError::Vector(VectorError::VectorProviderStatus {
            provider: "turbopuffer",
            status: 503,
            body: "unavailable".to_string(),
        }))
    })
    .await;
    assert!(matches!(transient, LegOutcome::Transient(_)));

    let fatal = run_leg::<Vec<LegCandidate>, _>(false, "vector", VECTOR_BUDGET, async {
        Err(RetrievalError::Vector(
            VectorError::TurbopufferUnavailable {
                storage_partition_id: "sp".to_string(),
            },
        ))
    })
    .await;
    assert!(matches!(fatal, LegOutcome::Fatal(_)));

    let timeout = run_leg::<Vec<LegCandidate>, _>(
        false,
        "vector",
        std::time::Duration::from_millis(1),
        async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(Vec::new())
        },
    )
    .await;
    assert!(matches!(timeout, LegOutcome::Timeout));
}

#[test]
fn reduce_leg_degrades_transient_and_timeout_but_aborts_fatal() {
    // Pins: F09 — a transient error or timeout degrades one leg to empty
    // (keeping peers), while a fatal error aborts. This is the degrade-keeps-
    // peer decision: mutating the Transient arm to return Err breaks this.
    let transient = reduce_leg::<Vec<LegCandidate>>(
        "vector",
        LegOutcome::Transient(RetrievalError::Vector(VectorError::VectorProviderStatus {
            provider: "turbopuffer",
            status: 503,
            body: "unavailable".to_string(),
        })),
    )
    .expect("a transient leg error must degrade, not abort");
    assert!(transient.value.is_empty());
    assert!(!transient.timed_out);

    let timeout = reduce_leg::<Vec<LegCandidate>>("vector", LegOutcome::Timeout)
        .expect("a timed-out leg must degrade, not abort");
    assert!(timeout.value.is_empty());
    assert!(timeout.timed_out);

    let completed = reduce_leg::<Vec<LegCandidate>>(
        "vector",
        LegOutcome::Completed(vec![leg_candidate(Uuid::from_u128(1))]),
    )
    .expect("a completed leg passes through");
    assert_eq!(completed.value.len(), 1);
    assert!(!completed.timed_out);

    let fatal = reduce_leg::<Vec<LegCandidate>>(
        "vector",
        LegOutcome::Fatal(RetrievalError::Scope(
            moa_core::error::MoaError::StorageError("rls setup failed".to_string()),
        )),
    );
    assert!(fatal.is_err(), "a fatal leg error must abort retrieval");
}

#[test]
fn retrieval_error_classification_matches_fatal_transient_table() {
    // Pins: F09 — RLS/privacy/scope/invalid-config errors are fatal; ordinary
    // backend/provider/network/query errors are transient.
    assert!(is_fatal_retrieval_error(&RetrievalError::Scope(
        moa_core::error::MoaError::StorageError("scope".to_string())
    )));
    assert!(is_fatal_retrieval_error(&RetrievalError::Graph(
        GraphError::RlsDenied
    )));
    assert!(is_fatal_retrieval_error(&RetrievalError::Graph(
        GraphError::MissingScope
    )));
    assert!(is_fatal_retrieval_error(&RetrievalError::Vector(
        VectorError::TurbopufferBaaRequired {
            storage_partition_id: "sp".to_string(),
        }
    )));
    assert!(is_fatal_retrieval_error(&RetrievalError::Vector(
        VectorError::DimensionMismatch {
            expected: 1024,
            actual: 768,
        }
    )));

    assert!(!is_fatal_retrieval_error(&RetrievalError::Vector(
        VectorError::VectorProviderStatus {
            provider: "turbopuffer",
            status: 503,
            body: "down".to_string(),
        }
    )));
    assert!(!is_fatal_retrieval_error(&RetrievalError::Vector(
        VectorError::ReembedInProgress {
            storage_partition_id: "sp".to_string(),
        }
    )));
    assert!(!is_fatal_retrieval_error(&RetrievalError::Graph(
        GraphError::GraphQuery("backend hiccup".to_string())
    )));
}

#[tokio::test]
async fn rerank_failure_falls_back_to_fused_pre_rerank_order() {
    // Pins: F09 — a reranker provider failure must not abort otherwise-usable
    // fused hits; it degrades to the fused pre-rerank order.
    let pool = PgPool::connect_lazy("postgres://unused")
        .expect("lazy pool construction should not connect");
    let retriever = HybridRetriever::new(
        pool.clone(),
        Arc::new(EmptyGraph),
        lazy_pgvector_source(&pool),
    )
    .with_reranker(Arc::new(FailingReranker));
    let mut req = vector_request();
    req.k_final = 2;
    req.use_reranker = true;
    let first = hit(Uuid::from_u128(1), "workspace", 2.0);
    let second = hit(Uuid::from_u128(2), "workspace", 1.0);

    let out = retriever
        .rerank_hits(&req, &[first.clone(), second.clone()])
        .await
        .expect("reranker failure must degrade, not abort");

    assert_eq!(out.hits, vec![first, second]);
    assert!(
        out.scores.is_empty(),
        "a degraded reranker attributes no scores"
    );
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
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: String::new(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final,
        use_reranker,
        strategy: None,
        as_of: None,
        ranking_reference_time: Some(Utc::now()),
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
    }
}

fn vector_request() -> RetrievalRequest {
    RetrievalRequest {
        cleared_barriers: Default::default(),
        query_text: "deployment runbook".to_string(),
        query_embedding: vec![0.0; 1024],
        ..empty_corpus_request(5, false)
    }
}

fn leg_candidate(uid: Uuid) -> LegCandidate {
    LegCandidate {
        uid,
        score: 1.0,
        similarity: None,
    }
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
    ) -> moa_core::error::Result<Vec<RerankHit>> {
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
    ) -> moa_core::error::Result<Vec<RerankHit>> {
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
        similarity: None,
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
            pii_class: SensitivityClass::None,
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
        context_window: Vec::new(),
    }
}

fn fact_hit_with_spo(
    uid: Uuid,
    score: f64,
    subject: &str,
    predicate: &str,
    object: &str,
) -> RetrievalHit {
    let mut fact = hit(uid, "contact", score);
    fact.node.properties_summary = Some(serde_json::json!({
        "subject": subject,
        "predicate": predicate,
        "object": object,
    }));
    fact
}

#[test]
fn restated_fact_defers_to_distinct_fact_but_era_objects_survive() {
    // Pins: final selection defers a fact restating identical content
    // (same subject/predicate/object) so a distinct lower-ranked fact can
    // take the slot, while update-era facts (same subject/predicate,
    // different object) are never collapsed.
    let hits = vec![
        fact_hit_with_spo(Uuid::from_u128(1), 1.0, "component", "depends_on", "lib-a"),
        fact_hit_with_spo(Uuid::from_u128(2), 0.9, "component", "depends_on", "lib-a"),
        fact_hit_with_spo(
            Uuid::from_u128(3),
            0.8,
            "component",
            "depends_on",
            "lib-old",
        ),
        fact_hit_with_spo(Uuid::from_u128(4), 0.7, "lib-a", "owned_by", "team-search"),
    ];

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::AnchoredRescue);

    assert_eq!(
        selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        vec![Uuid::from_u128(1), Uuid::from_u128(3), Uuid::from_u128(4)],
        "restatement must defer; era variant and second hop must be kept"
    );
}

#[test]
fn cap_rejected_hit_does_not_block_a_later_fact_with_the_same_content() {
    // Pins: a hit rejected by the per-object cap must not record its
    // content facet as selected — a later distinct hit carrying the same
    // content must still win a slot over deferred duplicates.
    let object = Uuid::from_u128(0xA);
    let chunk_fact = |uid: u128, score: f64, subject: &str| {
        let mut fact = fact_hit_with_spo(Uuid::from_u128(uid), score, subject, "covers", "topic");
        fact.knowledge_chunk = Some(KnowledgeChunkHydration {
            chunk_uid: Uuid::from_u128(uid + 0x100),
            document_version_uid: Uuid::from_u128(uid + 0x200),
            object_uid: object,
            chunk_hash: format!("chunk-{uid}"),
            ordinal: 0,
            heading_path: Vec::new(),
            text: subject.to_string(),
            token_count: 4,
            metadata: Value::Null,
            source_uri: None,
            source_title: None,
            object_type: "article".to_string(),
            context_window: Vec::new(),
        });
        fact
    };
    let hits = vec![
        chunk_fact(1, 1.0, "alpha"),
        chunk_fact(2, 0.9, "beta"),
        // Third hit for the same knowledge object: rejected by the cap.
        chunk_fact(3, 0.8, "gamma"),
        // Same content as an already-selected hit: a true duplicate.
        fact_hit_with_spo(Uuid::from_u128(4), 0.7, "alpha", "covers", "topic"),
        // Same content as the cap-rejected hit: must still be selectable.
        fact_hit_with_spo(Uuid::from_u128(5), 0.6, "gamma", "covers", "topic"),
    ];

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::AnchoredRescue);

    assert_eq!(
        selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(5)],
        "the cap-rejected gamma facet must not defer the later gamma fact"
    );
}

#[test]
fn duplicate_facets_backfill_when_fewer_distinct_facets_than_k() {
    // Pins: when the candidate pool has fewer distinct content facets than
    // k, deferred duplicates backfill in fused order instead of returning
    // fewer hits than the undiversified selection.
    let hits = vec![
        fact_hit_with_spo(Uuid::from_u128(1), 1.0, "s", "p", "o"),
        fact_hit_with_spo(Uuid::from_u128(2), 0.9, "s", "p", "o"),
        fact_hit_with_spo(Uuid::from_u128(3), 0.8, "s", "p", "o"),
    ];

    let selected = select_final_hits_for_policy(hits, &[], 3, GraphRetrievalPolicy::AnchoredRescue);

    assert_eq!(
        selected.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        vec![Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)],
        "duplicates must backfill in fused order"
    );
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
        context_window: Vec::new(),
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
    ) -> moa_core::error::Result<Vec<RerankHit>> {
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

struct FailingReranker;

#[async_trait]
impl Reranker for FailingReranker {
    async fn rerank(
        &self,
        _model: &str,
        _query: &str,
        _documents: &[String],
        _top_n: usize,
    ) -> moa_core::error::Result<Vec<RerankHit>> {
        Err(moa_core::error::MoaError::ProviderError(
            "injected rerank failure".to_string(),
        ))
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

    async fn get_node(&self, _uid: Uuid) -> std::result::Result<Option<NodeIndexRow>, GraphError> {
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
        _scoring: &moa_memory_graph::GraphWalkScoring,
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

#[test]
fn evidence_floor_drops_lexically_unsupported_hits_but_keeps_graph_hits() {
    // Pins: the injection evidence floor is absolute (not rank-relative), so a
    // window of nearest-neighbor noise empties out while graph-admitted hits
    // (whose evidence is the anchored path) always survive. Default-off: the
    // 2026-07-11 hermetic sweep showed a lexical-only floor trades recall@4
    // for precision, so it must never fire unless explicitly configured.
    let query = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: "what is the deploy target for checkout".to_string(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 4,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
    };
    let mut supported = fact_hit_with_spo(
        Uuid::from_u128(1),
        1.0,
        "checkout service",
        "deploy target",
        "us-east-1",
    );
    supported
        .node
        .properties_summary
        .as_mut()
        .expect("summary json")
        .as_object_mut()
        .expect("summary object")
        .insert(
            "summary".to_string(),
            serde_json::json!("checkout deploy target is us-east-1"),
        );
    let unrelated = fact_hit_with_spo(Uuid::from_u128(2), 0.9, "lunch order", "was", "sandwiches");
    let mut graph_only = hit(Uuid::from_u128(3), "contact", 0.8);
    graph_only.legs = LegSources {
        graph: true,
        vector: false,
        lexical: false,
    };
    // Paraphrased evidence: zero token overlap with the query, but the vector
    // leg surfaced it with high cosine similarity. Dropping hits like this is
    // what sank the lexical-only floor, so similarity must clear the floor.
    let mut paraphrased = fact_hit_with_spo(
        Uuid::from_u128(5),
        0.85,
        "release destination",
        "is",
        "primary region",
    );
    paraphrased.similarity = Some(0.82);
    // Low-similarity vector noise must not be rescued by its similarity.
    let mut weak_neighbor =
        fact_hit_with_spo(Uuid::from_u128(6), 0.7, "lunch order", "was", "salad");
    weak_neighbor.similarity = Some(0.12);

    let mut hits = vec![supported, unrelated, graph_only, paraphrased, weak_neighbor];
    let config = RankingConfig {
        min_hit_evidence: 0.25,
        ..RankingConfig::default()
    };
    apply_injection_evidence_floor(&mut hits, &config, &query);

    assert_eq!(
        hits.iter().map(|hit| hit.uid).collect::<Vec<_>>(),
        vec![Uuid::from_u128(1), Uuid::from_u128(3), Uuid::from_u128(5)],
        "lexical-evidence, graph, and high-similarity hits stay; noise drops"
    );

    // With both the per-hit floor and window abstention disabled, the stage
    // must be a no-op even for pure noise.
    let mut noise = vec![fact_hit_with_spo(
        Uuid::from_u128(4),
        0.9,
        "lunch order",
        "was",
        "sandwiches",
    )];
    let disabled = RankingConfig {
        min_hit_evidence: 0.0,
        ..RankingConfig::default()
    };
    apply_injection_evidence_floor(&mut noise, &disabled, &query);
    assert_eq!(noise.len(), 1);

    // A request carrying the production abstain window policy, by contrast,
    // abstains on that same evidence-free window (live-calibrated threshold,
    // 2026-07-11). The threshold now rides the request, not the config.
    let mut default_noise = vec![fact_hit_with_spo(
        Uuid::from_u128(7),
        0.9,
        "lunch order",
        "was",
        "sandwiches",
    )];
    let abstaining = RetrievalRequest {
        cleared_barriers: Default::default(),
        window_policy: crate::retrieval::EvidenceWindowPolicy {
            rerank_window: 0,
            abstain_below_window_evidence: 0.68,
        },
        ..query.clone()
    };
    apply_injection_evidence_floor(&mut default_noise, &RankingConfig::default(), &abstaining);
    assert!(
        default_noise.is_empty(),
        "request abstain policy must clear an evidence-free window"
    );
}

#[test]
fn window_abstention_clears_low_evidence_windows_but_spares_supported_and_graph_windows() {
    // Pins: whole-window abstention fires only when the BEST window evidence is
    // below the threshold and nothing is graph-admitted — an unanswerable query
    // returns nothing instead of nearest-of-nothing noise, while one supported
    // hit (or any graph-admitted hit) keeps the whole window alive.
    let query = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: "what is the deploy target for checkout".to_string(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 4,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        // The whole-window abstain threshold rides the request per retrieval
        // path, not the shared ranking config.
        window_policy: crate::retrieval::EvidenceWindowPolicy {
            rerank_window: 0,
            abstain_below_window_evidence: 0.68,
        },
    };
    let config = RankingConfig::default();
    let noise = |uid: u128, sim: f64| {
        let mut hit = fact_hit_with_spo(Uuid::from_u128(uid), 0.9, "lunch order", "was", "salad");
        hit.similarity = Some(sim);
        hit
    };

    // Unanswerable: every hit is sub-threshold noise — the window clears.
    let mut unanswerable = vec![noise(1, 0.62), noise(2, 0.60), noise(3, 0.55)];
    apply_injection_evidence_floor(&mut unanswerable, &config, &query);
    assert!(unanswerable.is_empty(), "low-evidence window must abstain");

    // Answerable: one high-similarity hit keeps the whole window (per-hit
    // floor is off, so the low-evidence neighbors stay too).
    let mut answerable = vec![noise(4, 0.85), noise(5, 0.60)];
    apply_injection_evidence_floor(&mut answerable, &config, &query);
    assert_eq!(answerable.len(), 2, "a supported window must not abstain");

    // Graph-admitted evidence exempts the window from abstention entirely.
    let mut graph_backed = vec![noise(6, 0.55)];
    graph_backed[0].legs = LegSources {
        graph: true,
        vector: false,
        lexical: false,
    };
    apply_injection_evidence_floor(&mut graph_backed, &config, &query);
    assert_eq!(
        graph_backed.len(),
        1,
        "graph-admitted windows never abstain"
    );
}

#[test]
fn default_window_policy_never_abstains() {
    // Pins: the evidence window is request-scoped. A request carrying the
    // default (off) EvidenceWindowPolicy is never abstained. This is the guard
    // against the 2026-07-11 clamp where a retriever-global window policy cut a
    // knowledge-lane retrieval down to the memory-lane window.
    let query = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: "what is the deploy target for checkout".to_string(),
        query_embedding: Vec::new(),
        scope: tenant_scope(),
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 4,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: crate::retrieval::EvidenceWindowPolicy::default(),
    };
    // The window policy rides the request; the ranking config no longer carries
    // any window knob that could drive abstention.
    let config = RankingConfig::default();
    let mut noise = vec![fact_hit_with_spo(
        Uuid::from_u128(1),
        0.9,
        "lunch order",
        "was",
        "sandwiches",
    )];

    apply_injection_evidence_floor(&mut noise, &config, &query);

    assert_eq!(noise.len(), 1, "a default request policy must not abstain");
}
