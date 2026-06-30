// Memory budget gate fixture support.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use moa_eval::memory_eval::{MemoryRetrievalEvalReport, ProbeResult, ProbeType};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

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
