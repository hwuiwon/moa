// Memory eval metric aggregation fixture support.

use std::error::Error;
use std::path::Path;

use moa_brain::retrieval::LexicalBackend;
use moa_eval::kernel::{CostLedger, ProviderProvenance};
use moa_eval::memory_eval::{
    BinaryProbeOutcome, BootstrapConfig, CandidateLegs, EntityFragmentationCounts,
    ExtractionPrecisionCounts, GoldNodeRecord, GoldPiiStatus, GoldResolutionReport,
    GoldResolutionStatus, MemoryRetrievalEvalReport, MetricSummary, ProbeResult, ProbeType,
    RetrievedCandidate, aggregate_retrieval_eval_from_counts,
    aggregate_retrieval_eval_from_diagnostic_counts, aggregate_retrieval_eval_with_diagnostics,
    aggregate_retrieval_eval_with_extraction_precision, benjamini_hochberg, mcnemar_paired_test,
};
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
