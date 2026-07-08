#[path = "memory_eval_support/common.rs"]
mod common;
use common::*;

include!("memory_eval_support/metrics.rs");

#[test]
fn retrieval_metrics_aggregate_exact_small_fixture() {
    // Pins: retrieval metrics compute exact recall, ranking, safety, PII, temporal, and per-leg values.
    let report = aggregate_retrieval_eval_from_counts(
        4,
        5,
        retrieval_metric_probe_results(),
        BootstrapConfig {
            resamples: 200,
            seed: 17,
        },
    );

    assert_metric(report.metrics.ingestion_coverage, 4.0, 5, 0.8);
    assert_metric(report.metrics.pre_rerank_recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.pre_rerank_recall_at_25, 4.0, 5, 0.8);
    assert_metric(report.metrics.post_rerank_recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.recall_at_4, 3.0, 5, 0.6);
    assert_metric(report.metrics.recall_at_25, 4.0, 5, 0.8);
    assert_metric(report.metrics.mrr, 2.7, 5, 0.54);
    assert_metric(
        report.metrics.ndcg_at_4,
        2.650_920_929_807_133,
        5,
        0.530_184_185_961_426_6,
    );
    assert_metric(report.metrics.zero_recall_rate, 1.0, 5, 0.2);
    assert_metric(report.metrics.answer_faithfulness, 4.0, 5, 0.8);
    assert_metric(report.metrics.abstention_correctness, 1.0, 2, 0.5);
    assert_eq!(report.metrics.cross_user_leak_count, 1);
    assert_eq!(report.metrics.pii_unredacted_count, 0);
    assert_metric(report.metrics.pii_redaction_rate, 1.0, 1, 1.0);
    assert_metric(report.metrics.temporal_as_of_accuracy, 0.0, 1, 0.0);
    assert_metric(report.metrics.temporal_parse_rate, 1.0, 1, 1.0);
    assert_eq!(report.metrics.temporal_parse_mismatch_count, 0);
    assert_metric(
        report.metrics.per_leg_recall.graph,
        2.0,
        6,
        0.333_333_333_333_333_3,
    );
    assert_metric(
        report.metrics.per_leg_recall.vector,
        2.0,
        6,
        0.333_333_333_333_333_3,
    );
    assert_metric(report.metrics.per_leg_recall.lexical, 3.0, 6, 0.5);
    assert_eq!(report.metrics.p95_retrieval_latency_ms, 0);
    assert_eq!(
        report.cross_user_leak_probe_ids,
        vec!["probe-cross-user-leak".to_string()]
    );

    let first_candidate = &report.probe_results[0].candidates[0];
    assert_eq!(first_candidate.fact_id.as_deref(), Some("fact-runtime"));
    assert_eq!(
        first_candidate.legs,
        CandidateLegs {
            graph: true,
            vector: true,
            lexical: false,
            lexical_backend: None,
        },
        "candidate conversion must copy RetrievalHit.legs into serializable output"
    );

    let recall_bootstrap = report
        .bootstrap
        .iter()
        .find(|interval| interval.metric_name == "retrieval.recall_at_4")
        .expect("recall@4 bootstrap report exists");
    assert_eq!(recall_bootstrap.resamples, 200);
    assert_eq!(recall_bootstrap.seed, 17);
    assert_eq!(recall_bootstrap.cluster_count, 3);
    assert_eq!(recall_bootstrap.observation_count, 5);
    assert_close(recall_bootstrap.mean, 0.6);
}

#[test]
fn retrieval_metrics_report_lexical_backend_recall_split() -> TestResult {
    // Pins: eval reports distinguish the lexical backend without changing lexical-leg recall.
    let report = aggregate_retrieval_eval_from_counts(
        3,
        3,
        vec![
            backend_probe(
                "probe-postgres",
                "fact-postgres",
                LexicalBackend::PostgresTsvector,
                0x1_200,
            ),
            backend_probe(
                "probe-turbopuffer",
                "fact-turbopuffer",
                LexicalBackend::TurbopufferBm25,
                0x1_300,
            ),
            backend_probe("probe-mixed", "fact-mixed", LexicalBackend::Mixed, 0x1_400),
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 29,
        },
    );

    assert_metric(report.metrics.per_leg_recall.lexical, 3.0, 3, 1.0);
    assert_metric(
        report.metrics.per_lexical_backend_recall.postgres_tsvector,
        1.0,
        3,
        1.0 / 3.0,
    );
    assert_metric(
        report.metrics.per_lexical_backend_recall.turbopuffer_bm25,
        1.0,
        3,
        1.0 / 3.0,
    );
    assert_metric(
        report.metrics.per_lexical_backend_recall.mixed,
        1.0,
        3,
        1.0 / 3.0,
    );

    let value = serde_json::to_value(&report)?;
    assert_eq!(
        value["probe_results"][0]["candidates"][0]["legs"]["lexical_backend"],
        "postgres_tsvector"
    );
    assert_eq!(
        value["metrics"]["per_lexical_backend_recall"]["turbopuffer_bm25"]["numerator"],
        1.0
    );
    Ok(())
}

fn backend_probe(probe_id: &str, fact_id: &str, backend: LexicalBackend, uid: u128) -> ProbeResult {
    ProbeResult {
        probe_id: probe_id.to_string(),
        user_id: "user-backend".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&[fact_id]),
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: Vec::new(),
        candidates: vec![RetrievedCandidate {
            uid: Uuid::from_u128(uid),
            rank: 1,
            score: 1.0,
            fact_id: Some(fact_id.to_string()),
            equivalent_fact_ids: Vec::new(),
            legs: CandidateLegs {
                graph: false,
                vector: false,
                lexical: true,
                lexical_backend: Some(backend),
            },
        }],
        post_rerank_candidates: None,
        retrieval_latency_ms: 0,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    }
}

#[test]
fn memory_eval_report_serializes_probe_graph_harm_path() -> TestResult {
    // Pins: memory eval reports explain graph harm per probe with seed and path identity.
    use std::collections::BTreeMap;

    use moa_brain::retrieval::{
        GraphCandidateCounts, GraphPathTrace, GraphRetrievalDiagnostics, GraphRetrievalPolicy,
        GraphSeedDiagnostics, GraphSeedSource,
    };
    use moa_eval::memory_eval::{
        GraphImpact, MemoryGraphDiagnostics, ProbeGraphComparison, ProbeGraphPathDiagnostic,
    };

    let seed_uid = Uuid::from_u128(0x6_0000);
    let harmful_uid = Uuid::from_u128(0x7_0001);
    let graph_candidates = metric_candidates(
        0x7_0000,
        &[
            CandidateSpec {
                fact_id: Some("fact-wrong"),
                legs: legs(true, false, false),
            },
            CandidateSpec {
                fact_id: Some("fact-right"),
                legs: legs(false, true, false),
            },
        ],
    );
    let graph_off_candidates = metric_candidates(
        0x8_0000,
        &[CandidateSpec {
            fact_id: Some("fact-right"),
            legs: legs(false, true, false),
        }],
    );
    let graph_diagnostics = GraphRetrievalDiagnostics {
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
        source_object_ranking: moa_brain::retrieval::SourceObjectRankingDiagnostics::default(),
        graph_latency_ms: 7,
        raw_path_count: 1,
    };
    let mut report = memory_budget_report(vec![ProbeResult {
        probe_id: "probe-graph-hurt".to_string(),
        user_id: "user-graph".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-right"]),
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: Vec::new(),
        candidates: graph_candidates,
        post_rerank_candidates: None,
        retrieval_latency_ms: 11,
        answer_faithful: Some(false),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
        graph_diagnostics: Some(graph_diagnostics),
        graph_comparison: Some(ProbeGraphComparison {
            impact: GraphImpact::Hurt,
            relevant_rank_with_graph: Some(2),
            relevant_rank_without_graph: Some(1),
            rank_delta_with_minus_without: Some(1),
            graph_off_candidates,
            top_harmful_graph_paths: vec![ProbeGraphPathDiagnostic {
                seed_uid,
                seed_source: Some(GraphSeedSource::BroadFallback),
                candidate_uid: harmful_uid,
                candidate_rank_with_graph: Some(1),
                candidate_fact_id: Some("fact-wrong".to_string()),
                hop: 1,
                edge_labels: vec!["RELATED_TO".to_string()],
            }],
            graph_off_retrieval_latency_ms: 3,
        }),
    }]);
    report.graph_diagnostics = MemoryGraphDiagnostics::from_probe_results(&report.probe_results);

    let value = serde_json::to_value(&report)?;

    assert_eq!(value["graph_diagnostics"]["raw_path_count"], 1);
    assert_eq!(value["graph_diagnostics"]["graph_hurt_count"], 1);
    assert_eq!(
        value["probe_results"][0]["graph_comparison"]["impact"],
        "hurt"
    );
    assert_eq!(
        value["probe_results"][0]["graph_comparison"]["top_harmful_graph_paths"][0]["seed_uid"],
        seed_uid.to_string()
    );
    assert_eq!(
        value["probe_results"][0]["graph_comparison"]["top_harmful_graph_paths"][0]["seed_source"],
        "broad_fallback"
    );
    assert_eq!(
        value["probe_results"][0]["graph_comparison"]["top_harmful_graph_paths"][0]["candidate_uid"],
        harmful_uid.to_string()
    );
    assert_eq!(
        value["probe_results"][0]["graph_comparison"]["top_harmful_graph_paths"][0]["candidate_fact_id"],
        "fact-wrong"
    );
    assert_eq!(
        value["probe_results"][0]["graph_diagnostics"]["path_traces"][0]["edge_labels"][0],
        "RELATED_TO"
    );
    Ok(())
}

#[test]
fn extraction_precision_counts_unmapped_fact_nodes_as_spurious() {
    // Pins: stored Fact nodes that do not map to ledger facts lower extraction precision.
    let report = aggregate_retrieval_eval_from_diagnostic_counts(
        2,
        3,
        1,
        2,
        ExtractionPrecisionCounts {
            mapped_fact_nodes: 2,
            total_fact_nodes: 5,
        },
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
    );

    assert_metric(report.metrics.ingestion_coverage, 2.0, 3, 2.0 / 3.0);
    assert_metric(report.metrics.scope_match_rate, 1.0, 2, 0.5);
    assert_metric(report.metrics.extraction_precision, 2.0, 5, 0.4);
}

#[test]
fn entity_fragmentation_counts_active_entities_over_distinct_mentions() {
    // Pins: entity fragmentation reports stored active Entity nodes over normalized ledger mentions.
    let report = aggregate_retrieval_eval_with_diagnostics(
        &GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
        ExtractionPrecisionCounts::default(),
        EntityFragmentationCounts {
            active_entity_nodes: 5,
            distinct_ledger_mentions: 4,
        },
    );

    assert_metric(report.metrics.entity_fragmentation, 5.0, 4, 1.25);
}

#[test]
fn scope_match_rate_slices_partition_the_overall_tally() {
    // Pins: scope-match slices expose contact/tenant drift without changing the overall tally.
    fn scope_record(fact_id: &str, expected_scope: &str, stored_scope: &str) -> GoldNodeRecord {
        GoldNodeRecord {
            fact_id: fact_id.to_string(),
            node_uids: vec![Uuid::now_v7()],
            scope: Some(stored_scope.to_string()),
            active: true,
            valid_from: Some(utc("2026-05-07T12:00:00Z")),
            valid_to: None,
            resolution_status: GoldResolutionStatus::Resolved,
            expected_scope: expected_scope.to_string(),
            expected_valid_from: utc("2026-05-07T12:00:00Z"),
            expected_valid_to: None,
            pii_status: GoldPiiStatus::NotExpected,
            stored_pii_classes: vec!["none".to_string()],
            supersedes: Vec::new(),
            superseded_by: Vec::new(),
            supersession_chain: vec![fact_id.to_string()],
            nodes: Vec::new(),
        }
    }

    let gold = GoldResolutionReport {
        ingest_reports: Vec::new(),
        records: vec![
            scope_record("fact-contact-match", "contact", "contact"),
            scope_record("fact-contact-miss", "contact", "tenant"),
            scope_record("fact-tenant-match", "tenant", "tenant"),
            scope_record("fact-tenant-miss", "tenant", "contact"),
        ],
    };

    let report = aggregate_retrieval_eval_with_extraction_precision(
        &gold,
        Vec::new(),
        BootstrapConfig {
            resamples: 25,
            seed: 43,
        },
        ExtractionPrecisionCounts::default(),
    );

    assert_metric(report.metrics.scope_match_rate, 2.0, 4, 0.5);
    assert_metric(report.metrics.scope_match_rate_contact, 1.0, 2, 0.5);
    assert_metric(report.metrics.scope_match_rate_tenant, 1.0, 2, 0.5);
    assert_eq!(
        report.metrics.scope_match_rate_contact.numerator
            + report.metrics.scope_match_rate_tenant.numerator,
        report.metrics.scope_match_rate.numerator
    );
    assert_eq!(
        report.metrics.scope_match_rate_contact.denominator
            + report.metrics.scope_match_rate_tenant.denominator,
        report.metrics.scope_match_rate.denominator
    );
}

#[test]
fn reranker_metrics_track_pre_post_windows_and_p95_latency() {
    // Pins: reranker eval reports pre-rerank recall, post-rerank recall, nDCG@4, and p95 latency separately.
    let report = aggregate_retrieval_eval_from_counts(
        2,
        2,
        vec![
            ProbeResult {
                probe_id: "probe-reranked-into-final-window".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::PointRecall,
                expected_fact_ids: fact_ids(&["fact-reranked"]),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0xe00,
                    &[
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(true, false, false),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(false, true, false),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(false, false, true),
                        },
                        CandidateSpec {
                            fact_id: None,
                            legs: legs(true, true, false),
                        },
                        CandidateSpec {
                            fact_id: Some("fact-reranked"),
                            legs: legs(false, true, true),
                        },
                    ],
                ),
                post_rerank_candidates: Some(metric_candidates(
                    0xf00,
                    &[CandidateSpec {
                        fact_id: Some("fact-reranked"),
                        legs: legs(false, true, true),
                    }],
                )),
                retrieval_latency_ms: 2_400,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
                graph_diagnostics: None,
                graph_comparison: None,
            },
            ProbeResult {
                probe_id: "probe-stable-top-hit".to_string(),
                user_id: "user-bob".to_string(),
                probe_type: ProbeType::PointRecall,
                expected_fact_ids: fact_ids(&["fact-stable"]),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0x1000,
                    &[CandidateSpec {
                        fact_id: Some("fact-stable"),
                        legs: legs(true, false, false),
                    }],
                ),
                post_rerank_candidates: Some(metric_candidates(
                    0x1100,
                    &[CandidateSpec {
                        fact_id: Some("fact-stable"),
                        legs: legs(true, false, false),
                    }],
                )),
                retrieval_latency_ms: 100,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
                graph_diagnostics: None,
                graph_comparison: None,
            },
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 31,
        },
    );

    assert_metric(report.metrics.pre_rerank_recall_at_4, 1.0, 2, 0.5);
    assert_metric(report.metrics.pre_rerank_recall_at_25, 2.0, 2, 1.0);
    assert_metric(report.metrics.post_rerank_recall_at_4, 2.0, 2, 1.0);
    assert_metric(report.metrics.recall_at_4, 2.0, 2, 1.0);
    assert_metric(report.metrics.ndcg_at_4, 2.0, 2, 1.0);
    assert_eq!(report.metrics.p95_retrieval_latency_ms, 2_400);
}

#[test]
fn retrieval_metrics_stats_pin_bootstrap_mcnemar_and_bh() {
    // Pins: statistical comparisons resample user clusters and correct paired binary tests.
    let report = aggregate_retrieval_eval_from_counts(
        4,
        5,
        retrieval_metric_probe_results(),
        BootstrapConfig {
            resamples: 200,
            seed: 17,
        },
    );
    let recall_bootstrap = report
        .bootstrap
        .iter()
        .find(|interval| interval.metric_name == "retrieval.recall_at_4")
        .expect("recall@4 bootstrap report exists");
    assert_close(recall_bootstrap.lower, 0.5);
    assert_close(recall_bootstrap.upper, 1.0);

    let comparison_a = mcnemar_paired_test(
        "retrieval.recall_at_4",
        &binary_outcomes("abcdef", |_| false),
        &binary_outcomes("abcdef", |_| true),
    );
    assert_eq!(comparison_a.total_pairs, 6);
    assert_eq!(comparison_a.control_only_successes, 0);
    assert_eq!(comparison_a.treatment_only_successes, 6);
    assert_close(comparison_a.p_value, 0.03125);

    let comparison_b = mcnemar_paired_test(
        "retrieval.mrr",
        &binary_outcomes("abcdef", |index| index == 0),
        &binary_outcomes("abcdef", |index| index > 0),
    );
    assert_eq!(comparison_b.control_only_successes, 1);
    assert_eq!(comparison_b.treatment_only_successes, 5);
    assert_close(comparison_b.p_value, 0.21875);

    let comparison_c = mcnemar_paired_test(
        "retrieval.ndcg_at_4",
        &binary_outcomes("abcdef", |index| index < 3),
        &binary_outcomes("abcdef", |index| index >= 3),
    );
    assert_eq!(comparison_c.control_only_successes, 3);
    assert_eq!(comparison_c.treatment_only_successes, 3);
    assert_close(comparison_c.p_value, 1.0);

    let corrected = benjamini_hochberg(
        vec![
            comparison_b.clone(),
            comparison_c.clone(),
            comparison_a.clone(),
        ],
        0.1,
    );
    assert_eq!(corrected[0].metric_name, "retrieval.mrr");
    assert_close(corrected[0].adjusted_p_value, 0.328125);
    assert!(!corrected[0].significant);
    assert_eq!(corrected[1].metric_name, "retrieval.ndcg_at_4");
    assert_close(corrected[1].adjusted_p_value, 1.0);
    assert!(!corrected[1].significant);
    assert_eq!(corrected[2].metric_name, "retrieval.recall_at_4");
    assert_close(corrected[2].adjusted_p_value, 0.09375);
    assert!(corrected[2].significant);
}

#[test]
fn retrieval_metrics_security_counts_ignore_non_cross_user_blocked_leaks_and_count_pii_unredacted()
{
    // Pins: only cross-user isolation probes contribute hard leak counts, and PII probe redaction failures are counted.
    let report = aggregate_retrieval_eval_from_counts(
        3,
        3,
        vec![
            ProbeResult {
                probe_id: "probe-latest-ordinary-blocked-leak".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::LatestValueAfterUpdate,
                expected_fact_ids: fact_ids(&["fact-current"]),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: fact_ids(&["fact-old"]),
                candidates: metric_candidates(
                    0x800,
                    &[
                        CandidateSpec {
                            fact_id: Some("fact-old"),
                            legs: legs(true, false, false),
                        },
                        CandidateSpec {
                            fact_id: Some("fact-current"),
                            legs: legs(false, true, false),
                        },
                    ],
                ),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(true),
                abstention_correct: None,
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
                graph_diagnostics: None,
                graph_comparison: None,
            },
            ProbeResult {
                probe_id: "probe-cross-user-clean".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::CrossUserIsolation,
                expected_fact_ids: Vec::new(),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
                candidates: Vec::new(),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(true),
                abstention_correct: Some(true),
                pii_redacted: None,
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
                graph_diagnostics: None,
                graph_comparison: None,
            },
            ProbeResult {
                probe_id: "probe-pii-unredacted".to_string(),
                user_id: "user-alice".to_string(),
                probe_type: ProbeType::PiiRedaction,
                expected_fact_ids: fact_ids(&["fact-phone"]),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                candidates: metric_candidates(
                    0x900,
                    &[CandidateSpec {
                        fact_id: Some("fact-phone"),
                        legs: legs(false, false, true),
                    }],
                ),
                post_rerank_candidates: None,
                retrieval_latency_ms: 0,
                answer_faithful: Some(false),
                abstention_correct: None,
                pii_redacted: Some(false),
                temporal_as_of_correct: None,
                temporal_filter_parsed: None,
                temporal_filter_matches_as_of: None,
                preference_context_hit: None,
                graph_diagnostics: None,
                graph_comparison: None,
            },
        ],
        BootstrapConfig {
            resamples: 200,
            seed: 19,
        },
    );

    assert_eq!(report.metrics.cross_user_leak_count, 0);
    assert_eq!(report.cross_user_leak_probe_ids, Vec::<String>::new());
    assert_eq!(report.metrics.pii_unredacted_count, 1);
    assert_metric(report.metrics.pii_redaction_rate, 0.0, 1, 0.0);
}

#[test]
fn retrieval_metrics_flatten_round_trips_checked_in_baseline() -> TestResult {
    // Pins: splitting core metrics into a flattened Rust field does not change report JSON.
    let baseline_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/eval/baselines/memory-retrieval-pr-baseline.json");
    let raw = std::fs::read_to_string(&baseline_path)?;
    let before: serde_json::Value = serde_json::from_str(&raw)?;
    let report: MemoryRetrievalEvalReport = serde_json::from_str(&raw)?;
    let after = serde_json::to_value(report)?;

    assert_eq!(after, before);
    Ok(())
}

#[test]
fn report_serializes_cost_and_providers_sections() -> TestResult {
    // Pins: live eval reports carry spend and provider provenance in the current report shape.
    let mut report = memory_budget_report(Vec::new());
    report.cost = Some(CostLedger::new(5.0));
    report.providers = Some(ProviderProvenance {
        lane: "live".to_string(),
        embedding_model: "embed-v4.0".to_string(),
        embedding_model_version: 1,
        extractor_model: "gpt-5.4-mini".to_string(),
        extraction_prompt_version: Some("v2".to_string()),
        merge_verifier_model: "gpt-5.4-mini".to_string(),
        merge_prompt_version: Some("v1".to_string()),
        reranker_model: "rerank-v4.0-fast".to_string(),
    });

    let value = serde_json::to_value(&report)?;

    assert_eq!(value["graph_expansion_policy"], "current");
    assert_eq!(value["cost"]["budget_usd"], 5.0);
    assert_eq!(value["providers"]["lane"], "live");
    assert_eq!(value["providers"]["embedding_model"], "embed-v4.0");
    let old_report = serde_json::json!({
        "manifest": report.manifest,
        "candidate_k": report.candidate_k,
        "final_k": report.final_k,
        "reranker_enabled": false,
        "metrics": report.metrics,
        "probe_results": [],
        "bootstrap": report.bootstrap,
        "cross_user_leak_probe_ids": [],
        "gold_resolution": report.gold_resolution
    });
    let parsed: MemoryRetrievalEvalReport = serde_json::from_value(old_report)?;
    assert_eq!(parsed.cost, None);
    assert_eq!(parsed.providers, None);
    assert!(!parsed.aborted_over_budget);
    Ok(())
}

#[test]
fn temporal_parse_rate_aggregates_over_temporal_probes_only() {
    // Pins: parser diagnostics count temporal probes only and separate wrong-date parses.
    let report = aggregate_retrieval_eval_from_counts(
        3,
        3,
        vec![
            parse_metric_probe(
                "probe-temporal-parsed",
                ProbeType::TemporalAsOf,
                Some(true),
                Some(true),
            ),
            parse_metric_probe(
                "probe-temporal-missing",
                ProbeType::TemporalAsOf,
                Some(false),
                None,
            ),
            parse_metric_probe(
                "probe-temporal-mismatch",
                ProbeType::TemporalAsOf,
                Some(true),
                Some(false),
            ),
            parse_metric_probe(
                "probe-point-with-diagnostic-noise",
                ProbeType::PointRecall,
                Some(false),
                Some(false),
            ),
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 41,
        },
    );

    assert_metric(report.metrics.temporal_parse_rate, 2.0, 3, 2.0 / 3.0);
    assert_eq!(report.metrics.temporal_parse_mismatch_count, 1);
}

#[test]
fn graded_ndcg_penalizes_misranking_a_high_grade_memory_below_a_low_grade_one() {
    // Pins: graded nDCG distinguishes ranking quality that binary nDCG cannot —
    // retrieving both expected facts scores 1.0 on the binary metric in either
    // order, while the graded metric drops when the grade-3 fact ranks below
    // the grade-1 fact.
    let grades = std::collections::BTreeMap::from([
        ("fact-primary".to_string(), 3_u8),
        ("fact-context".to_string(), 1_u8),
    ]);
    let candidate = |rank: usize, fact_id: &str, uid: u128| RetrievedCandidate {
        uid: Uuid::from_u128(uid),
        rank,
        score: 1.0 / rank as f64,
        fact_id: Some(fact_id.to_string()),
        equivalent_fact_ids: Vec::new(),
        legs: CandidateLegs::default(),
    };
    let probe = |candidates: Vec<RetrievedCandidate>| ProbeResult {
        probe_id: "probe-graded".to_string(),
        user_id: "user-graded".to_string(),
        probe_type: ProbeType::MultiHop,
        expected_fact_ids: fact_ids(&["fact-primary", "fact-context"]),
        expected_fact_grades: grades.clone(),
        blocked_fact_ids: Vec::new(),
        candidates,
        post_rerank_candidates: None,
        retrieval_latency_ms: 0,
        answer_faithful: None,
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    };

    let well_ranked = probe(vec![
        candidate(1, "fact-primary", 0x2_100),
        candidate(2, "fact-context", 0x2_200),
    ]);
    let misranked = probe(vec![
        candidate(1, "fact-context", 0x2_200),
        candidate(2, "fact-primary", 0x2_100),
    ]);

    assert_close(well_ranked.graded_ndcg_at(10).expect("graded ndcg"), 1.0);
    let misranked_graded = misranked.graded_ndcg_at(10).expect("graded ndcg");
    assert!(
        misranked_graded < 1.0,
        "grade-3 below grade-1 must lose graded nDCG: {misranked_graded}"
    );
    assert_close(misranked.ndcg_at(10).expect("binary ndcg"), 1.0);
    // Absent grades default to the maximum, matching binary behavior.
    let mut ungraded = misranked.clone();
    ungraded.expected_fact_grades.clear();
    assert_close(ungraded.graded_ndcg_at(10).expect("graded ndcg"), 1.0);
}

#[test]
fn per_probe_type_slices_report_sliced_means_with_dispersion() {
    // Pins: core ranking metrics are sliced by probe type with count and
    // standard error, so budget gates can read the slice a change's mechanism
    // moves instead of a global mean that hides per-intent regressions.
    let hit_probe = |probe_id: &str, probe_type: ProbeType, fact: &str, uid: u128, hit: bool| {
        let mut probe = backend_probe(probe_id, fact, LexicalBackend::PostgresTsvector, uid);
        probe.probe_type = probe_type;
        if !hit {
            probe.candidates.clear();
        }
        probe
    };
    let report = aggregate_retrieval_eval_from_counts(
        4,
        4,
        vec![
            hit_probe(
                "probe-point-hit",
                ProbeType::PointRecall,
                "fact-a",
                0x3_100,
                true,
            ),
            hit_probe(
                "probe-multi-hit",
                ProbeType::MultiHop,
                "fact-b",
                0x3_200,
                true,
            ),
            hit_probe(
                "probe-multi-miss",
                ProbeType::MultiHop,
                "fact-c",
                0x3_300,
                false,
            ),
        ],
        BootstrapConfig {
            resamples: 25,
            seed: 31,
        },
    );

    let slices = &report.metrics.per_probe_type;
    assert_eq!(slices.len(), 2);
    let point = slices.get("point_recall").expect("point_recall slice");
    assert_eq!(point.probes, 1);
    let point_recall = point.recall_at_4.expect("point recall stat");
    assert_close(point_recall.mean, 1.0);
    assert_eq!(point_recall.count, 1);
    assert_close(point_recall.std_error, 0.0);
    let multi = slices.get("multi_hop").expect("multi_hop slice");
    assert_eq!(multi.probes, 2);
    let multi_recall = multi.recall_at_4.expect("multi recall stat");
    assert_close(multi_recall.mean, 0.5);
    assert_eq!(multi_recall.count, 2);
    assert!(
        multi_recall.std_error > 0.0,
        "two-probe slice with variance must report a nonzero standard error"
    );

    // The slice serializes under a stable dotted path for --min-metric gates.
    let value = serde_json::to_value(&report).expect("serialize report");
    assert_close(
        value["metrics"]["per_probe_type"]["multi_hop"]["recall_at_4"]["mean"]
            .as_f64()
            .expect("slice mean"),
        0.5,
    );
    assert!(
        value["metrics"]["graded_ndcg_at_10"]["denominator"]
            .as_u64()
            .is_some(),
        "graded nDCG@10 must serialize as a headline metric"
    );
}
