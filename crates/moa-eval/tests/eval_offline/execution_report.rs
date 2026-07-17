//! Offline tests for the strict execution-eval report wire and arithmetic.

use std::collections::BTreeMap;

use moa_eval::execution::{
    ExecutionEvalCaseResultV1, ExecutionEvalLaneV1, ExecutionEvalReportV1,
    ExecutionInvariantSpecV1, ExecutionJudgeCalibrationStatusV1,
};
use moa_execution::state::{ExecutionRunStatus, ExecutionTaskStatus};
use serde_json::{Value, json};

use super::execution_snapshot::eval_snapshot;

#[test]
fn execution_report_rejects_unknown_missing_renamed_and_duplicate_fields_offline() {
    // Pins: the V1 report wire fails closed instead of tolerating schema drift.
    let report = valid_report();
    let value = serde_json::to_value(&report).expect("valid report should serialize");

    let mut unknown = value.clone();
    unknown
        .as_object_mut()
        .expect("report JSON should be an object")
        .insert("unexpected".to_string(), json!(true));
    assert!(serde_json::from_value::<ExecutionEvalReportV1>(unknown).is_err());

    let mut missing = value.clone();
    missing
        .as_object_mut()
        .expect("report JSON should be an object")
        .remove("lane");
    assert!(serde_json::from_value::<ExecutionEvalReportV1>(missing).is_err());

    let mut renamed = value;
    let object = renamed
        .as_object_mut()
        .expect("report JSON should be an object");
    let schema_version = object
        .remove("schema_version")
        .expect("fixture should have schema_version");
    object.insert("version".to_string(), schema_version);
    assert!(serde_json::from_value::<ExecutionEvalReportV1>(renamed).is_err());

    let encoded = serde_json::to_string(&report).expect("valid report should serialize");
    let duplicated = encoded.replacen('{', "{\"schema_version\":1,", 1);
    assert!(serde_json::from_str::<ExecutionEvalReportV1>(&duplicated).is_err());
}

#[test]
fn execution_report_rejects_version_identity_hash_metric_and_count_drift_offline() {
    // Pins: version, case identity, corpus hash, finite metric, and aggregate arithmetic are hard validation gates.
    let mut report = valid_report();
    report.schema_version = 2;
    assert!(report.validate().is_err());

    let mut report = valid_report();
    report.cases.push(report.cases[0].clone());
    report.metrics.total_cases = 2;
    report.metrics.passed_cases = 2;
    report.metrics.impossible_cases = 2;
    assert!(report.validate().is_err());

    let mut report = valid_report();
    report.corpus_hashes.clear();
    assert!(report.validate().is_err());

    let mut report = valid_report();
    report.metrics.mutation_score = Some(f64::NAN);
    assert!(report.validate().is_err());

    let mut report = valid_report();
    report.metrics.total_cases = 99;
    assert!(report.validate().is_err());
}

#[test]
fn execution_report_recomputes_false_completion_from_typed_case_rows_offline() {
    // Pins: false-completion numerator and denominator cannot be edited independently of case evidence.
    let partial = eval_snapshot(
        ExecutionRunStatus::Partial,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let completed = eval_snapshot(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let spec = ExecutionInvariantSpecV1::MapCoverage {
        node_id: "research".to_string(),
        expected_keys: vec!["issuer-a".to_string(), "issuer-b".to_string()],
        require_all_when_completed: true,
    };
    let partial_case = ExecutionEvalCaseResultV1::evaluate(
        "silent-incomplete-partial",
        &partial,
        std::slice::from_ref(&spec),
        10,
    )
    .expect("partial case should evaluate");
    let completed_case =
        ExecutionEvalCaseResultV1::evaluate("silent-incomplete-completed", &completed, &[spec], 10)
            .expect("completed case should evaluate");
    let report = ExecutionEvalReportV1::new(
        ExecutionEvalLaneV1::OfflinePr,
        BTreeMap::from([("execution-fixture".to_string(), "a".repeat(64))]),
        vec![42],
        1,
        ExecutionJudgeCalibrationStatusV1::Unavailable,
        None,
        vec![partial_case, completed_case],
    )
    .expect("consistent report should validate");

    assert_eq!(report.metrics.impossible_cases, 2);
    assert_eq!(report.metrics.execution_false_completions, 1);
    assert_eq!(report.metrics.execution_false_completion_rate, Some(0.5));

    let mut tampered = report;
    tampered.cases[1].execution_false_completion = false;
    assert!(tampered.validate().is_err());
}

#[test]
fn execution_report_serialization_contains_hashes_not_raw_execution_payloads_offline() {
    // Pins: persisted reports contain terminal/final hashes and invariant summaries, never snapshot task payloads or raw events.
    let report = valid_report();
    let encoded = report
        .canonical_json()
        .expect("valid report should serialize canonically");
    let value: Value = serde_json::from_str(&encoded).expect("report JSON should parse");
    let case = &value["cases"][0];

    assert!(case.get("terminal_output_hash").is_some());
    assert!(case.get("final_response_hash").is_some());
    for forbidden in [
        "tasks",
        "task_output",
        "terminal_output",
        "final_response",
        "raw_event_body",
        "transcript",
    ] {
        assert!(
            case.get(forbidden).is_none(),
            "report case unexpectedly contains `{forbidden}`"
        );
    }
}

fn valid_report() -> ExecutionEvalReportV1 {
    let snapshot = eval_snapshot(
        ExecutionRunStatus::Partial,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let case = ExecutionEvalCaseResultV1::evaluate(
        "silent-incomplete-partial",
        &snapshot,
        &[ExecutionInvariantSpecV1::MapCoverage {
            node_id: "research".to_string(),
            expected_keys: vec!["issuer-a".to_string(), "issuer-b".to_string()],
            require_all_when_completed: true,
        }],
        10,
    )
    .expect("fixture case should evaluate");
    ExecutionEvalReportV1::new(
        ExecutionEvalLaneV1::OfflinePr,
        BTreeMap::from([("execution-fixture".to_string(), "a".repeat(64))]),
        vec![42],
        1,
        ExecutionJudgeCalibrationStatusV1::Unavailable,
        None,
        vec![case],
    )
    .expect("fixture report should validate")
}
