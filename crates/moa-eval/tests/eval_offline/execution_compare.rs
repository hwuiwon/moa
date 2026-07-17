//! Offline pairing, statistical gating, and mutation-report coverage.

use std::collections::BTreeMap;

use moa_eval::execution::{
    ExecutionEvalCaseResultV1, ExecutionEvalComparisonConfigV1, ExecutionEvalLaneV1,
    ExecutionEvalReportV1, ExecutionJudgeCalibrationStatusV1, compare_execution_eval_reports,
    mutation_report_from_outcomes,
};
use serde_json::json;

#[test]
fn execution_compare_refuses_corpus_seed_repetition_and_case_mismatch_offline() {
    // Pins: paired statistics are never computed over different experimental identities.
    let baseline = report(ExecutionEvalLaneV1::OfflinePr, 4, true);

    let mut mismatch = baseline.clone();
    mismatch
        .corpus_hashes
        .insert("routing".to_string(), "b".repeat(64));
    assert!(compare_execution_eval_reports(&baseline, &mismatch, config()).is_err());

    let mut mismatch = baseline.clone();
    mismatch.seeds = vec![99];
    assert!(compare_execution_eval_reports(&baseline, &mismatch, config()).is_err());

    let mut mismatch = baseline.clone();
    mismatch.repetitions = 5;
    assert!(compare_execution_eval_reports(&baseline, &mismatch, config()).is_err());

    let mut mismatch = baseline.clone();
    mismatch.cases[0].case_id = "different-case".to_string();
    assert!(compare_execution_eval_reports(&baseline, &mismatch, config()).is_err());
}

#[test]
fn execution_compare_deterministically_detects_significant_live_regression_offline() {
    // Pins: one sample cannot gate live quality, while a paired repeated regression can.
    let baseline = report(ExecutionEvalLaneV1::NightlyLive, 20, true);
    let candidate = report(ExecutionEvalLaneV1::NightlyLive, 20, false);
    let comparison = compare_execution_eval_reports(&baseline, &candidate, config())
        .expect("paired live reports should compare");

    assert_eq!(comparison.paired_cases, 20);
    assert_eq!(comparison.pass_rate_delta.mean, -1.0);
    assert!(comparison.pass_comparison.significant);
    assert!(comparison.significant_pass_regression);
    assert!(comparison.gate_failed);

    let one_baseline = report(ExecutionEvalLaneV1::NightlyLive, 1, true);
    let one_candidate = report(ExecutionEvalLaneV1::NightlyLive, 1, false);
    let one = compare_execution_eval_reports(&one_baseline, &one_candidate, config())
        .expect("one paired case should still produce a trend report");
    assert!(!one.pass_comparison.significant);
    assert!(!one.gate_failed);
}

#[test]
fn execution_compare_reports_improvement_and_numeric_deltas_stably_offline() {
    // Pins: candidate-minus-baseline direction is consistent for pass, cost, latency, and tasks.
    let baseline = report(ExecutionEvalLaneV1::OfflinePr, 8, false);
    let mut candidate = report(ExecutionEvalLaneV1::OfflinePr, 8, true);
    for case in &mut candidate.cases {
        case.cost_microusd = 50;
        case.latency_ms = 75;
        case.task_count = 2;
    }
    let comparison = compare_execution_eval_reports(&baseline, &candidate, config())
        .expect("paired deterministic reports should compare");
    assert_eq!(comparison.pass_rate_delta.mean, 1.0);
    assert_eq!(comparison.cost_microusd_delta.mean, -50.0);
    assert_eq!(comparison.latency_ms_delta.mean, -25.0);
    assert_eq!(comparison.task_count_delta.mean, -1.0);
    assert!(!comparison.significant_pass_regression);
    assert!(!comparison.gate_failed);
}

#[test]
fn execution_mutation_report_excludes_unviable_and_retains_missed_timeouts_offline() {
    // Pins: timeouts stay in the denominator but never count as caught, and triage names survive.
    let report = mutation_report_from_outcomes(&json!({
        "outcomes": [
            {"summary": "CaughtMutant", "mutant": "coverage-guard"},
            {"summary": "CaughtMutant", "mutant": "budget-guard"},
            {"summary": "MissedMutant", "mutant": "authorization-guard"},
            {"summary": "Timeout", "mutant": "generation-fence"},
            {"summary": "Unviable", "mutant": "equivalent-build"},
            {"summary": "Success", "scenario": "baseline"}
        ]
    }))
    .expect("strict mutation outcomes should score");
    assert_eq!(report.caught, 2);
    assert_eq!(report.missed, 1);
    assert_eq!(report.timeouts, 1);
    assert_eq!(report.unviable, 1);
    assert_eq!(report.viable, 4);
    assert_eq!(report.mutation_score, 0.5);
    assert_eq!(report.missed_mutants, vec!["authorization-guard"]);
    assert_eq!(report.timeout_mutants, vec!["generation-fence"]);
}

fn report(lane: ExecutionEvalLaneV1, count: usize, passed: bool) -> ExecutionEvalReportV1 {
    let cases = (0..count)
        .map(|index| ExecutionEvalCaseResultV1 {
            case_id: format!("case-{index:03}"),
            passed,
            contract_omission: None,
            contract_score: None,
            impossible_case: false,
            execution_false_completion: false,
            observed_run_status: None,
            observed_route: None,
            route_provenance: None,
            invariants: Vec::new(),
            cost_microusd: 100,
            latency_ms: 100,
            task_count: 3,
            terminal_output_hash: None,
            final_response_hash: None,
        })
        .collect();
    ExecutionEvalReportV1::new(
        lane,
        BTreeMap::from([("routing".to_string(), "a".repeat(64))]),
        vec![42],
        1,
        ExecutionJudgeCalibrationStatusV1::Unavailable,
        None,
        cases,
    )
    .expect("comparison fixture report should validate")
}

fn config() -> ExecutionEvalComparisonConfigV1 {
    ExecutionEvalComparisonConfigV1 {
        bootstrap: moa_eval::kernel::BootstrapConfig {
            resamples: 512,
            seed: 7,
        },
        false_discovery_rate: 0.05,
        practical_pass_rate_regression: 0.10,
    }
}
