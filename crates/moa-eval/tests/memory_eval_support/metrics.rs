// Memory eval metric aggregation fixture support.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::path::Path;

use chrono::{DateTime, Utc};
use moa_brain::retrieval::{LegSources, LexicalBackend, RetrievalHit, SourceTier};
use moa_eval::kernel::{CostLedger, ProviderProvenance};
use moa_eval::memory_eval::runner::QueryRewriteClassMetrics;
use moa_eval::memory_eval::{
    BinaryProbeOutcome, BootstrapConfig, CORPUS_SCHEMA_VERSION, CandidateLegs, CorpusManifest,
    CorpusProfile, EntityFragmentationCounts, ExtractionPrecisionCounts, GoldNodeRecord,
    GoldPiiStatus, GoldResolutionReport, GoldResolutionStatus, GraphExpansionEvalPolicy,
    MemoryRetrievalEvalReport, MetricSummary, ProbeResult, ProbeType, QueryRewritePolicy,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate, TranscriptStyle,
    aggregate_retrieval_eval_from_counts, aggregate_retrieval_eval_from_diagnostic_counts,
    aggregate_retrieval_eval_with_diagnostics,
    aggregate_retrieval_eval_with_extraction_precision, benjamini_hochberg,
    candidates_from_retrieval_hits, mcnemar_paired_test,
};
use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn retrieval_metric_probe_results() -> Vec<ProbeResult> {
    vec![
        ProbeResult {
            probe_id: "probe-runtime".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PointRecall,
            expected_fact_ids: fact_ids(&["fact-runtime"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x100,
                &[CandidateSpec {
                    fact_id: Some("fact-runtime"),
                    legs: legs(true, true, false),
                }],
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
        },
        ProbeResult {
            probe_id: "probe-rank-five".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-rank-five"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x200,
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
                        fact_id: Some("fact-rank-five"),
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
        },
        ProbeResult {
            probe_id: "probe-multi-hop".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-service-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x300,
                &[
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(true, false, false),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-service-owner"),
                        legs: legs(false, false, true),
                    },
                    CandidateSpec {
                        fact_id: None,
                        legs: legs(false, true, true),
                    },
                    CandidateSpec {
                        fact_id: Some("fact-runbook"),
                        legs: legs(true, false, true),
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
        },
        ProbeResult {
            probe_id: "probe-temporal-miss".to_string(),
            user_id: "user-bob".to_string(),
            probe_type: ProbeType::TemporalAsOf,
            expected_fact_ids: fact_ids(&["fact-temporal-old"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x400,
                &[CandidateSpec {
                    fact_id: Some("fact-temporal-new"),
                    legs: legs(true, true, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(false),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: Some(false),
            temporal_filter_parsed: Some(true),
            temporal_filter_matches_as_of: Some(true),
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-pii-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0x500,
                &[CandidateSpec {
                    fact_id: Some("fact-pii-phone"),
                    legs: legs(false, false, true),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(true),
            abstention_correct: None,
            pii_redacted: Some(true),
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-abstains".to_string(),
            user_id: "user-casey".to_string(),
            probe_type: ProbeType::Abstention,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: Vec::new(),
            candidates: Vec::new(),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(true),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-secret"]),
            candidates: metric_candidates(
                0x700,
                &[CandidateSpec {
                    fact_id: Some("fact-secret"),
                    legs: legs(true, false, false),
                }],
            ),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: None,
            abstention_correct: Some(false),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
    ]
}

#[derive(Debug, Clone, Copy)]
struct CandidateSpec {
    fact_id: Option<&'static str>,
    legs: LegSources,
}

fn metric_candidates(base: u128, specs: &[CandidateSpec]) -> Vec<RetrievedCandidate> {
    let mut fact_ids_by_uid = HashMap::new();
    let hits = specs
        .iter()
        .enumerate()
        .map(|(index, spec)| {
            let uid = Uuid::from_u128(base + index as u128 + 1);
            if let Some(fact_id) = spec.fact_id {
                fact_ids_by_uid.insert(uid, fact_id.to_string());
            }
            RetrievalHit {
                uid,
                score: 1.0 / (index + 1) as f64,
                legs: spec.legs,
                lexical_backend: None,
                source_tier: SourceTier::UserMemory,
                knowledge_chunk: None,
                node: metric_node(uid),
            }
        })
        .collect::<Vec<_>>();
    candidates_from_retrieval_hits(&hits, &fact_ids_by_uid, &HashMap::new())
}

fn metric_node(uid: Uuid) -> NodeIndexRow {
    NodeIndexRow {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some("metrics-storage-partition".to_string()),
        contact_id: Some("metrics-contact".to_string()),
        scope: "tenant".to_string(),
        name: format!("metric-node-{uid}"),
        pii_class: PiiClass::None,
        valid_to: None,
        valid_from: utc("2026-05-01T00:00:00Z"),
        properties_summary: None,
        last_accessed_at: utc("2026-05-02T00:00:00Z"),
        quality_score: 0.5,
    }
}

fn parse_metric_probe(
    probe_id: &str,
    probe_type: ProbeType,
    temporal_filter_parsed: Option<bool>,
    temporal_filter_matches_as_of: Option<bool>,
) -> ProbeResult {
    ProbeResult {
        probe_id: probe_id.to_string(),
        user_id: "user-parser".to_string(),
        probe_type,
        expected_fact_ids: fact_ids(&["fact-parser"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0x1_0000,
            &[CandidateSpec {
                fact_id: Some("fact-parser"),
                legs: legs(false, false, true),
            }],
        ),
        post_rerank_candidates: None,
        retrieval_latency_ms: 0,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: (probe_type == ProbeType::TemporalAsOf).then_some(true),
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit: None,
    }
}

fn legs(graph: bool, vector: bool, lexical: bool) -> LegSources {
    LegSources {
        graph,
        vector,
        lexical,
    }
}

fn fact_ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn assert_metric(summary: MetricSummary, numerator: f64, denominator: usize, value: f64) {
    assert_close(summary.numerator, numerator);
    assert_eq!(summary.denominator, denominator);
    assert_close(summary.value, value);
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1.0e-12,
        "expected {expected}, got {actual}"
    );
}

fn binary_outcomes(
    probe_suffixes: &str,
    success_for_index: impl Fn(usize) -> bool,
) -> Vec<BinaryProbeOutcome> {
    probe_suffixes
        .chars()
        .enumerate()
        .map(|(index, suffix)| BinaryProbeOutcome {
            probe_id: format!("probe-{suffix}"),
            success: success_for_index(index),
        })
        .collect()
}

fn memory_budget_report(probe_results: Vec<ProbeResult>) -> MemoryRetrievalEvalReport {
    memory_budget_report_with_reranker(probe_results, false)
}


fn memory_budget_report_with_reranker(
    probe_results: Vec<ProbeResult>,
    reranker_enabled: bool,
) -> MemoryRetrievalEvalReport {
    let retrieval = aggregate_retrieval_eval_from_counts(
        3,
        3,
        probe_results,
        BootstrapConfig {
            resamples: 25,
            seed: 23,
        },
    );
    MemoryRetrievalEvalReport {
        manifest: CorpusManifest {
            version: CORPUS_SCHEMA_VERSION,
            corpus_id: "memory-budget-fixture".to_string(),
            profile: CorpusProfile::Pr,
            description: "Hermetic budget gate fixture.".to_string(),
            seeds: vec![1, 2, 3],
            transcript_style: TranscriptStyle::Marked,
        },
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled,
        query_rewrite_policy: QueryRewritePolicy::Gated,
        graph_expansion_policy: GraphExpansionEvalPolicy::Current,
        query_rewrite_call_count: 0,
        query_rewrite_skip_count: 0,
        query_rewrite_call_rate: 0.0,
        query_rewrite_p50_latency_ms: 0,
        query_rewrite_p95_latency_ms: 0,
        query_rewrite_input_tokens: 0,
        query_rewrite_output_tokens: 0,
        query_rewrite_est_usd: 0.0,
        retrieval_plus_rewrite_p95_latency_ms: retrieval.metrics.p95_retrieval_latency_ms,
        query_rewrite_by_class: BTreeMap::from([(
            "exact_identifier".to_string(),
            QueryRewriteClassMetrics {
                total_count: 1,
                call_count: 0,
                skip_count: 1,
                call_rate: 0.0,
            },
        )]),
        aborted_over_budget: false,
        cost: None,
        providers: None,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: GoldResolutionReport {
            ingest_reports: Vec::new(),
            records: Vec::new(),
        },
        consolidation: None,
    }
}


fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}
