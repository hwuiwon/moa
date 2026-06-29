// Memory budget gate fixture support.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{DateTime, Utc};
use moa_brain::retrieval::{LegSources, RetrievalHit, SourceTier};
use moa_eval::memory_eval::runner::QueryRewriteClassMetrics;
use moa_eval::memory_eval::{
    BootstrapConfig, CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, GoldResolutionReport,
    GraphExpansionEvalPolicy, MemoryRetrievalEvalReport, ProbeResult, ProbeType, QueryRewritePolicy,
    RETRIEVAL_EVAL_CANDIDATE_K, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate, TranscriptStyle,
    aggregate_retrieval_eval_from_counts, candidates_from_retrieval_hits,
};
use moa_memory_graph::{NodeIndexRow, NodeLabel, PiiClass};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

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

fn memory_budget_probe_results(cross_user_leak: bool) -> Vec<ProbeResult> {
    let cross_user_candidates = if cross_user_leak {
        metric_candidates(
            0xb00,
            &[CandidateSpec {
                fact_id: Some("fact-bob-secret"),
                legs: legs(true, false, false),
            }],
        )
    } else {
        Vec::new()
    };

    vec![
        ProbeResult {
            probe_id: "probe-latest-ordinary-blocked-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::LatestValueAfterUpdate,
            expected_fact_ids: fact_ids(&["fact-current"]),
            blocked_fact_ids: fact_ids(&["fact-old"]),
            candidates: metric_candidates(
                0xa00,
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
        },
        ProbeResult {
            probe_id: "probe-cross-user-leak".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: fact_ids(&["fact-bob-secret"]),
            candidates: cross_user_candidates,
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(!cross_user_leak),
            abstention_correct: Some(!cross_user_leak),
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-pii-redacted".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::PiiRedaction,
            expected_fact_ids: fact_ids(&["fact-phone"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(
                0xc00,
                &[CandidateSpec {
                    fact_id: Some("fact-phone"),
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
    ]
}

fn reranker_recall_regression_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-regresses-recall".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe00,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe10,
            &[CandidateSpec {
                fact_id: None,
                legs: legs(false, true, false),
            }],
        )),
        retrieval_latency_ms: 100,
        answer_faithful: Some(false),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn reranker_latency_without_gain_probe_results() -> Vec<ProbeResult> {
    vec![ProbeResult {
        probe_id: "probe-reranker-slow-without-gain".to_string(),
        user_id: "user-alice".to_string(),
        probe_type: ProbeType::PointRecall,
        expected_fact_ids: fact_ids(&["fact-owner"]),
        blocked_fact_ids: Vec::new(),
        candidates: metric_candidates(
            0xe20,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        ),
        post_rerank_candidates: Some(metric_candidates(
            0xe30,
            &[CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            }],
        )),
        retrieval_latency_ms: 2_501,
        answer_faithful: Some(true),
        abstention_correct: None,
        pii_redacted: None,
        temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
    }]
}

fn memory_budget_regression_probe_results(full_recall: bool) -> Vec<ProbeResult> {
    let candidate_specs = if full_recall {
        vec![
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
            CandidateSpec {
                fact_id: Some("fact-runbook"),
                legs: legs(false, true, false),
            },
        ]
    } else {
        vec![
            CandidateSpec {
                fact_id: None,
                legs: legs(false, false, true),
            },
            CandidateSpec {
                fact_id: Some("fact-owner"),
                legs: legs(true, false, false),
            },
        ]
    };

    vec![
        ProbeResult {
            probe_id: "probe-regression-multi-hop".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::MultiHop,
            expected_fact_ids: fact_ids(&["fact-owner", "fact-runbook"]),
            blocked_fact_ids: Vec::new(),
            candidates: metric_candidates(0xd00, &candidate_specs),
            post_rerank_candidates: None,
            retrieval_latency_ms: 0,
            answer_faithful: Some(full_recall),
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            temporal_filter_parsed: None,
            temporal_filter_matches_as_of: None,
            preference_context_hit: None,
        },
        ProbeResult {
            probe_id: "probe-regression-cross-user-clean".to_string(),
            user_id: "user-alice".to_string(),
            probe_type: ProbeType::CrossUserIsolation,
            expected_fact_ids: Vec::new(),
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
        },
    ]
}

fn write_memory_budget_report(path: &Path, report: &MemoryRetrievalEvalReport) -> TestResult {
    let json = serde_json::to_vec_pretty(report)?;
    std::fs::write(path, json)?;
    Ok(())
}

fn run_memory_budget_gate(report_path: &Path, previous_path: Option<&Path>) -> TestResult<Output> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(workspace_root())
        .args([
            "run",
            "-p",
            "xtask",
            "--quiet",
            "--",
            "check-eval-budgets",
            "--suite",
            "memory_retrieval",
            "--max-regression-pct",
            "5",
            "--memory-eval-report",
        ])
        .arg(report_path);
    if let Some(previous_path) = previous_path {
        command.env("MOA_EVAL_PREVIOUS_MEMORY_REPORT", previous_path);
    } else {
        command.env_remove("MOA_EVAL_PREVIOUS_MEMORY_REPORT");
    }
    Ok(command.output()?)
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("moa-eval manifest lives under crates/moa-eval")
        .to_path_buf()
}

fn command_output_text(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}


fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}
