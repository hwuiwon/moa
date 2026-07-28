//! Runner behavior tests.

use super::*;
use crate::memory_eval::{
    CorpusProfile, TranscriptStyle, generate_memory_eval_corpus, write_memory_eval_corpus,
};
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
};
use moa_retrieval::planning::Strategy;

#[derive(Debug)]
struct PrDeterministicEmbedder;

#[async_trait]
impl EmbeddingProvider for PrDeterministicEmbedder {
    fn model_id(&self) -> &str {
        "memory-eval-deterministic-sha256-v1"
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    fn model_version(&self) -> i32 {
        7
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        Ok(vec![vec![0.0; VECTOR_DIMENSION]; inputs.len()])
    }
}

#[test]
fn graph_expansion_eval_lanes_follow_the_guarded_production_default() {
    // Pins: every memory-eval lane runs the guarded production policy; the
    // legacy broad-expansion A/B lane was deleted with the scored walk.
    assert_eq!(
        GraphExpansionEvalPolicy::Current.graph_retrieval_policy(),
        GraphRetrievalPolicy::AnchoredRescue
    );
    assert_eq!(
        GraphExpansionEvalPolicy::SkipExactDirect.graph_retrieval_policy(),
        GraphRetrievalPolicy::AnchoredRescue
    );
}

#[test]
fn graph_expansion_policy_skips_only_exact_direct_non_temporal_probes() {
    // Pins: the A/B policy only disables graph expansion for direct exact-anchor lookups.
    let planned = planned_for_policy(Strategy::Both, None);
    let req = request_for_policy("Who owns incident INC-123?");

    assert!(should_skip_graph_expansion_for_exact_direct_probe(
        &planned, &req
    ));
}

#[test]
fn graph_expansion_policy_keeps_graph_first_and_temporal_probes() {
    // Pins: multi-hop and historical probes still run graph expansion in the A/B lane.
    let graph_first = planned_for_policy(Strategy::GraphFirst, None);
    let temporal = planned_for_policy(
        Strategy::Both,
        Some(
            DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                .expect("test timestamp should parse")
                .with_timezone(&Utc),
        ),
    );
    let req = request_for_policy("What depends on incident INC-123?");

    assert!(!should_skip_graph_expansion_for_exact_direct_probe(
        &graph_first,
        &req
    ));
    assert!(!should_skip_graph_expansion_for_exact_direct_probe(
        &temporal, &req
    ));
}

#[test]
fn graph_expansion_policy_does_not_treat_contractions_as_exact_anchors() {
    // Pins: natural prose with apostrophes is not an exact-anchor lookup.
    let planned = planned_for_policy(Strategy::Both, None);
    let req = request_for_policy("What's failing in the deploy flow?");

    assert!(!should_skip_graph_expansion_for_exact_direct_probe(
        &planned, &req
    ));
}

#[test]
fn probe_graph_comparison_classifies_hurt_and_keeps_path_identity() {
    // Pins: memory eval graph A/B diagnostics keep the seed and path behind hurt probes.
    use crate::memory_eval::CandidateLegs;
    use moa_retrieval::retrieval::{
        GraphCandidateCounts, GraphPathTrace, GraphSeedDiagnostics, GraphSeedSource,
    };

    let seed_uid = Uuid::from_u128(0x4_0000);
    let harmful_uid = Uuid::from_u128(0x5_0001);
    let relevant_uid = Uuid::from_u128(0x5_0002);
    let graph_candidates = vec![
        RetrievedCandidate {
            uid: harmful_uid,
            rank: 1,
            score: 0.9,
            similarity: None,
            lexical_evidence: None,
            fact_id: Some("fact-wrong".to_string()),
            equivalent_fact_ids: Vec::new(),
            legs: CandidateLegs {
                graph: true,
                vector: false,
                lexical: false,
                lexical_backend: None,
            },
        },
        RetrievedCandidate {
            uid: relevant_uid,
            rank: 2,
            score: 0.8,
            similarity: None,
            lexical_evidence: None,
            fact_id: Some("fact-right".to_string()),
            equivalent_fact_ids: Vec::new(),
            legs: CandidateLegs {
                graph: false,
                vector: true,
                lexical: false,
                lexical_backend: None,
            },
        },
    ];
    let graph_off_candidates = vec![RetrievedCandidate {
        uid: relevant_uid,
        rank: 1,
        score: 1.0,
        similarity: None,
        lexical_evidence: None,
        fact_id: Some("fact-right".to_string()),
        equivalent_fact_ids: Vec::new(),
        legs: CandidateLegs {
            graph: false,
            vector: true,
            lexical: false,
            lexical_backend: None,
        },
    }];
    let diagnostics = GraphRetrievalDiagnostics {
        policy: GraphRetrievalPolicy::AnchoredRescue,
        seed_counts: GraphSeedDiagnostics {
            broad_fallback: 1,
            ..GraphSeedDiagnostics::default()
        },
        path_label_histogram: BTreeMap::from([("RELATED_TO".to_string(), 1)]),
        hop_histogram: BTreeMap::from([(1, 1)]),
        path_traces: vec![GraphPathTrace {
            seed_uid,
            seed_source: Some(GraphSeedSource::BroadFallback),
            candidate_uid: harmful_uid,
            hop: 1,
            edge_labels: vec!["RELATED_TO".to_string()],
            edge_directions: vec!["outgoing".to_string()],
        }],
        candidate_counts: GraphCandidateCounts {
            graph_only: 1,
            ..GraphCandidateCounts::default()
        },
        source_object_ranking: moa_retrieval::retrieval::SourceObjectRankingDiagnostics::default(),
        graph_latency_ms: 9,
        raw_path_count: 1,
    };

    let comparison = probe_graph_comparison(
        &["fact-right".to_string()],
        &graph_candidates,
        graph_off_candidates,
        &diagnostics,
        4,
    );

    assert_eq!(comparison.impact, GraphImpact::Hurt);
    assert_eq!(comparison.relevant_rank_with_graph, Some(2));
    assert_eq!(comparison.relevant_rank_without_graph, Some(1));
    assert_eq!(comparison.rank_delta_with_minus_without, Some(1));
    assert_eq!(comparison.top_harmful_graph_paths.len(), 1);
    let path = &comparison.top_harmful_graph_paths[0];
    assert_eq!(path.seed_uid, seed_uid);
    assert_eq!(path.seed_source, Some(GraphSeedSource::BroadFallback));
    assert_eq!(path.candidate_uid, harmful_uid);
    assert_eq!(path.candidate_rank_with_graph, Some(1));
    assert_eq!(path.candidate_fact_id.as_deref(), Some("fact-wrong"));
    assert_eq!(path.hop, 1);
    assert_eq!(path.edge_labels, vec!["RELATED_TO".to_string()]);
}

fn planned_for_policy(strategy: Strategy, temporal_filter: Option<DateTime<Utc>>) -> PlannedQuery {
    let scope = MemoryScope::Tenant {
        tenant_id: TenantId::new(),
    };
    PlannedQuery {
        strategy,
        seeds: Vec::new(),
        label_hint: None,
        scope: scope.clone(),
        temporal_filter,
    }
}

fn request_for_policy(query_text: &str) -> RetrievalRequest {
    RetrievalRequest {
        source_acl: moa_core::types::memory::SourceAclContext::empty(0),
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: query_text.to_string(),
        query_embedding: vec![0.0; VECTOR_DIMENSION],
        scope: MemoryScope::Tenant {
            tenant_id: TenantId::new(),
        },
        label_filter: None,
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: RETRIEVAL_EVAL_FINAL_K,
        use_reranker: false,
        strategy: Some(Strategy::Both),
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: true,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    }
}

fn ledger_fact(storage_partition_id: StoragePartitionId, fact_id: &str) -> LedgerFact {
    LedgerFact {
        storage_partition_id,
        user_id: UserId::new("user"),
        scope: ScopeTier::Tenant,
        fact_id: fact_id.to_string(),
        valid_from: Utc::now(),
        valid_to: None,
        subject: "eval".to_string(),
        predicate: "uses_embedder".to_string(),
        object: "deterministic".to_string(),
        answer: "Eval uses the deterministic embedder.".to_string(),
        supersedes: Vec::new(),
        restates: None,
        prior_uses: None,
        prior_successes: None,
        source_session_id: SessionId::new(),
        source_turn_seq: 1,
        pii_class: SensitivityClass::None,
        expected_redacted: false,
    }
}

#[test]
fn live_lane_validation_defers_credentials_to_configured_provider_builders() {
    // Pins: live eval option validation checks lane shape only; provider
    // credentials are loaded through MoaConfig and validated by provider builders.
    let options = MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
        .with_lane(EvalLane::Live);

    options.validate().expect("lane shape should be valid");
}

#[test]
fn pr_lane_refuses_live_only_flags() {
    // Pins: PR eval cannot accept budget-only live-lane flags and pretend it ran hermetically.
    let options = MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
        .with_budget_usd(1.0);

    let error = options
        .validate()
        .expect_err("PR lane with a live budget should fail");

    assert!(error.to_string().contains("--budget-usd"));
}

#[test]
fn parity_refuses_eval_only_graph_expansion_overrides() {
    // Pins: parity mode drives the production graph-expansion policy, so an
    // eval-only A/B override would silently measure a different system.
    let options = MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
        .with_parity(true)
        .with_graph_expansion_policy(GraphExpansionEvalPolicy::SkipExactDirect);

    let error = options
        .validate()
        .expect_err("parity with an eval-only graph expansion override should fail");

    assert!(error.to_string().contains("--parity"));

    MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
        .with_parity(true)
        .validate()
        .expect("parity with the production graph policy should be valid");
}

#[test]
fn entity_embedding_texts_include_redacted_mentions() {
    // Pins: hermetic eval fixture preload covers entity names after deterministic PII redaction.
    let mut texts = BTreeMap::new();

    insert_entity_embedding_texts(&mut texts, "ops@example.com");

    let keys = texts.into_keys().collect::<Vec<_>>();
    assert_eq!(keys, vec!["email redacted", "ops example com"]);
}

#[test]
fn rewrite_accounting_gates_by_query_class() {
    // Pins: gated PR rewrite policy records fewer calls than always and preserves exact controls.
    let mut always = QueryRewriteAccounting::new(QueryRewritePolicy::Always);
    let mut gated = QueryRewriteAccounting::new(QueryRewritePolicy::Gated);
    let explicit = Probe {
        probe_id: "explicit".to_string(),
        probe_type: ProbeType::PointRecall,
        storage_partition_id: StoragePartitionId::new("workspace"),
        user_id: moa_core::types::identifiers::UserId::new("user"),
        query: "Which runbook is required for deploy?".to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: "Use the tenant deploy runbook.".to_string(),
        expected_fact_ids: Vec::new(),
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: Vec::new(),
        as_of: None,
        expected_redacted: false,
    };
    let exact = Probe {
        probe_id: "exact".to_string(),
        query: "Find docs/runbook.md".to_string(),
        ..explicit.clone()
    };
    let multi_hop = Probe {
        probe_id: "multi-hop".to_string(),
        probe_type: ProbeType::MultiHop,
        query: "Which team owns the library that api depends on?".to_string(),
        ..explicit.clone()
    };

    for probe in [&explicit, &exact, &multi_hop] {
        always.record(probe);
        gated.record(probe);
    }
    let always = always.summary();
    let gated = gated.summary();

    assert_eq!(always.call_count, 3);
    assert_eq!(gated.call_count, 1);
    assert_eq!(gated.skip_count, 2);
    assert_eq!(
        gated
            .by_class
            .get("exact_identifier")
            .expect("exact class should be recorded")
            .call_count,
        0
    );
}

#[test]
fn retrieval_runner_reports_observed_signals_without_synthesizing_an_answer() {
    // Pins: a gold answer remains corpus metadata and never becomes retrieval-report answer quality.
    let probe = Probe {
        probe_id: "probe-point".to_string(),
        probe_type: ProbeType::PointRecall,
        storage_partition_id: StoragePartitionId::new("workspace"),
        user_id: moa_core::types::identifiers::UserId::new("user"),
        query: "Which repository does Alice use?".to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: "Alice uses repo-alpha.".to_string(),
        expected_fact_ids: vec!["fact-repository".to_string()],
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: Vec::new(),
        as_of: None,
        expected_redacted: false,
    };
    let candidate = RetrievedCandidate {
        uid: Uuid::from_u128(0x6_0001),
        rank: 1,
        score: 1.0,
        similarity: None,
        lexical_evidence: None,
        fact_id: Some("fact-repository".to_string()),
        equivalent_fact_ids: Vec::new(),
        legs: crate::memory_eval::CandidateLegs::default(),
    };

    let result = probe_result_for(ProbeResultInput {
        probe: &probe,
        candidates: vec![candidate.clone()],
        post_rerank_candidates: Some(vec![candidate]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &HashMap::new(),
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });

    assert_eq!(result.all_expected_found_at_4, Some(true));
    assert_eq!(result.forbidden_fact_absent_at_4, None);
    assert_eq!(result.retrieval_temporal_as_of_correct, None);
    assert_eq!(result.stored_pii_redacted, None);
    let serialized = serde_json::to_value(result).expect("probe result should serialize");
    assert!(serialized.get("answer").is_none());
    assert!(serialized.get("answer_faithful").is_none());
}

#[test]
fn retrieval_runner_derives_negative_temporal_and_stored_pii_signals() {
    // Pins: retrieval-only policy signals come from final candidates and stored gold records.
    let base_probe = Probe {
        probe_id: "probe-base".to_string(),
        probe_type: ProbeType::PointRecall,
        storage_partition_id: StoragePartitionId::new("workspace"),
        user_id: moa_core::types::identifiers::UserId::new("user"),
        query: "query".to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: "gold answer must not be used".to_string(),
        expected_fact_ids: Vec::new(),
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: Vec::new(),
        as_of: None,
        expected_redacted: false,
    };
    let candidate = |uid: u128, rank: usize, fact_id: &str| RetrievedCandidate {
        uid: Uuid::from_u128(uid),
        rank,
        score: 1.0 / rank as f64,
        similarity: None,
        lexical_evidence: None,
        fact_id: Some(fact_id.to_string()),
        equivalent_fact_ids: Vec::new(),
        legs: crate::memory_eval::CandidateLegs::default(),
    };

    let negative = Probe {
        probe_id: "probe-negative".to_string(),
        probe_type: ProbeType::CrossUserIsolation,
        expected_fact_ids: Vec::new(),
        blocked_fact_ids: vec!["fact-forbidden".to_string()],
        ..base_probe.clone()
    };
    let harmless = candidate(0x6_1001, 1, "fact-harmless");
    let negative_result = probe_result_for(ProbeResultInput {
        probe: &negative,
        candidates: vec![harmless.clone()],
        post_rerank_candidates: Some(vec![harmless]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &HashMap::new(),
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });
    assert_eq!(negative_result.all_expected_found_at_4, None);
    assert_eq!(negative_result.forbidden_fact_absent_at_4, Some(true));
    let forbidden_negative_candidate = candidate(0x6_1002, 1, "fact-forbidden");
    let leaking_negative_result = probe_result_for(ProbeResultInput {
        probe: &negative,
        candidates: vec![forbidden_negative_candidate.clone()],
        post_rerank_candidates: Some(vec![forbidden_negative_candidate]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &HashMap::new(),
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });
    assert_eq!(
        leaking_negative_result.forbidden_fact_absent_at_4,
        Some(false)
    );

    let multi_hop = Probe {
        probe_id: "probe-multi-hop".to_string(),
        probe_type: ProbeType::MultiHop,
        expected_fact_ids: vec!["fact-owner".to_string(), "fact-runbook".to_string()],
        ..base_probe.clone()
    };
    let partial_support = candidate(0x6_1501, 1, "fact-owner");
    let multi_hop_result = probe_result_for(ProbeResultInput {
        probe: &multi_hop,
        candidates: vec![partial_support.clone()],
        post_rerank_candidates: Some(vec![partial_support]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &HashMap::new(),
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });
    assert_eq!(
        multi_hop_result.all_expected_found_at_4,
        Some(false),
        "one of two support facts is fractional recall, not complete support"
    );

    let temporal = Probe {
        probe_id: "probe-temporal".to_string(),
        probe_type: ProbeType::TemporalAsOf,
        expected_fact_ids: vec!["fact-old".to_string()],
        blocked_fact_ids: vec!["fact-new".to_string()],
        as_of: Some(
            DateTime::parse_from_rfc3339("2026-01-05T00:00:00Z")
                .expect("test timestamp should parse")
                .with_timezone(&Utc),
        ),
        ..base_probe.clone()
    };
    let expected = candidate(0x6_2001, 1, "fact-old");
    let forbidden = candidate(0x6_2002, 2, "fact-new");
    let temporal_result = probe_result_for(ProbeResultInput {
        probe: &temporal,
        candidates: vec![expected.clone(), forbidden.clone()],
        post_rerank_candidates: Some(vec![expected, forbidden]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &HashMap::new(),
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });
    assert_eq!(temporal_result.all_expected_found_at_4, Some(true));
    assert_eq!(
        temporal_result.retrieval_temporal_as_of_correct,
        Some(false),
        "a blocked temporal version in the final window must fail as-of retrieval"
    );

    let pii = Probe {
        probe_id: "probe-pii".to_string(),
        probe_type: ProbeType::PiiRedaction,
        expected_fact_ids: vec!["fact-phone".to_string()],
        expected_redacted: true,
        ..base_probe
    };
    let expected_valid_from = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("test timestamp should parse")
        .with_timezone(&Utc);
    let gold_records = HashMap::from([(
        "fact-phone".to_string(),
        crate::memory_eval::GoldNodeRecord {
            fact_id: "fact-phone".to_string(),
            node_uids: vec![Uuid::from_u128(0x6_3001)],
            scope: Some("contact".to_string()),
            active: true,
            valid_from: Some(expected_valid_from),
            valid_to: None,
            resolution_status: crate::memory_eval::GoldResolutionStatus::Resolved,
            expected_scope: "contact".to_string(),
            expected_valid_from,
            expected_valid_to: None,
            pii_status: GoldPiiStatus::Unredacted,
            stored_pii_classes: vec!["phone".to_string()],
            supersedes: Vec::new(),
            superseded_by: Vec::new(),
            supersession_chain: vec!["fact-phone".to_string()],
            nodes: Vec::new(),
        },
    )]);
    let pii_candidate = candidate(0x6_3001, 1, "fact-phone");
    let pii_result = probe_result_for(ProbeResultInput {
        probe: &pii,
        candidates: vec![pii_candidate.clone()],
        post_rerank_candidates: Some(vec![pii_candidate]),
        retrieval_latency_ms: 0,
        gold_records_by_fact_id: &gold_records,
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    });
    assert_eq!(pii_result.all_expected_found_at_4, Some(true));
    assert_eq!(pii_result.stored_pii_redacted, Some(false));
}

#[tokio::test]
async fn live_lane_skips_fixture_coverage_check() {
    // Pins: live provider runs do not load or require hermetic embedding fixtures.
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate a small deterministic corpus");
    let temp = tempfile::tempdir().expect("tempdir should be created");
    let corpus_dir = temp.path().join("corpus");
    write_memory_eval_corpus(&corpus_dir, &corpus)
        .await
        .expect("corpus files should be written without embeddings.jsonl");

    let live = LoadedMemoryEvalCorpus::load_for_lane(&corpus_dir, EvalLane::Live)
        .await
        .expect("live lane should not load embeddings.jsonl");
    let pr = LoadedMemoryEvalCorpus::load_for_lane(&corpus_dir, EvalLane::Pr).await;
    let error = match pr {
        Ok(_) => panic!("PR lane should require embeddings.jsonl"),
        Err(error) => error,
    };

    assert_eq!(live.embeddings.len(), 0);
    assert!(error.to_string().contains("embeddings.jsonl"));
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and local Postgres"]
async fn eval_seed_sets_pr_embedder_state_before_ingestion_db_memory() {
    // Pins: eval ingestion partitions are configured to the active PR embedder before vector writes.
    let store = IsolatedEvalStore::create()
        .await
        .expect("create isolated eval store");
    let storage_partition_a =
        StoragePartitionId::new(format!("memory-eval-pr-seed-a-{}", Uuid::now_v7()));
    let storage_partition_b =
        StoragePartitionId::new(format!("memory-eval-pr-seed-b-{}", Uuid::now_v7()));
    let ledger = vec![
        ledger_fact(storage_partition_a.clone(), "fact-a"),
        ledger_fact(storage_partition_b.clone(), "fact-b"),
        ledger_fact(storage_partition_b.clone(), "fact-b-duplicate-partition"),
    ];

    seed_eval_storage_partition_embedder_state(store.pool(), &ledger, &PrDeterministicEmbedder)
        .await
        .expect("seed eval storage partition state");

    let storage_partition_ids = vec![
        tenant_id_from_storage_partition_id(&storage_partition_a).to_string(),
        tenant_id_from_storage_partition_id(&storage_partition_b).to_string(),
    ];
    let rows = sqlx::query(
        r#"
        SELECT storage_partition_id, embedding_model, embedding_model_version, embedding_dimension
        FROM moa.storage_partition_state
        WHERE storage_partition_id = ANY($1)
        ORDER BY storage_partition_id ASC
        "#,
    )
    .bind(&storage_partition_ids)
    .fetch_all(store.pool())
    .await
    .expect("read seeded storage partition state");

    assert_eq!(rows.len(), 2);
    for row in rows {
        let model: String = row.try_get("embedding_model").expect("model column");
        let version: i32 = row
            .try_get("embedding_model_version")
            .expect("model version column");
        let dimension: i32 = row
            .try_get("embedding_dimension")
            .expect("embedding dimension column");
        assert_eq!(model, "memory-eval-deterministic-sha256-v1");
        assert_ne!(model, "cohere-embed-v4");
        assert_ne!(model, "embed-v4.0");
        assert_eq!(version, 7);
        assert_eq!(dimension, VECTOR_DIMENSION as i32);
    }

    sqlx::query("DELETE FROM moa.storage_partition_state WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(store.pool())
        .await
        .expect("delete seeded storage partition state rows");
    store.cleanup().await.expect("cleanup isolated eval store");
}
