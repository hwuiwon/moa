//! Offline tests for deterministic execution invariant predicates.

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::CapabilityReference;
use moa_eval::execution::{
    ExecutionCapabilityCallObservation, ExecutionEvalCaseResult, ExecutionInvariantSpec,
    evaluate_invariants,
};
use moa_execution::state::{ExecutionRunStatus, ExecutionTaskStatus};

use super::execution_snapshot::{capability_ref, eval_snapshot};

#[test]
fn execution_invariants_evaluate_every_typed_variant_with_stable_ids_offline() {
    // Pins: every closed invariant variant produces a bounded typed result without transcript interpretation.
    let snapshot = eval_snapshot(
        ExecutionRunStatus::Partial,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let specs = vec![
        ExecutionInvariantSpec::TerminalStatusIn {
            statuses: vec![ExecutionRunStatus::Partial],
        },
        ExecutionInvariantSpec::MustNotComplete,
        ExecutionInvariantSpec::TaskCount {
            node_id: "research".to_string(),
            exact: 1,
        },
        ExecutionInvariantSpec::MapCoverage {
            node_id: "research".to_string(),
            expected_keys: vec!["issuer-a".to_string(), "issuer-b".to_string()],
            require_all_when_completed: true,
        },
        ExecutionInvariantSpec::CompletionCheckPassed {
            check_id: "coverage-check".to_string(),
        },
        ExecutionInvariantSpec::CompletionCheckFailed {
            check_id: "coverage-check".to_string(),
        },
        ExecutionInvariantSpec::TerminalGapContains {
            text: "coverage is incomplete".to_string(),
        },
        ExecutionInvariantSpec::BudgetWithinApproved,
        ExecutionInvariantSpec::ProgressMatchesTasks,
        ExecutionInvariantSpec::NoDuplicateLogicalEffects,
        ExecutionInvariantSpec::AllowedCapabilitiesOnly {
            references: vec![capability_ref()],
        },
        ExecutionInvariantSpec::CompletedTaskKeysPreserved {
            node_id: "research".to_string(),
            item_keys: vec!["issuer-a".to_string()],
        },
        ExecutionInvariantSpec::SessionEventCountAtMost {
            event_kind: "progress".to_string(),
            max: 1,
        },
        ExecutionInvariantSpec::NoRawTaskOutputEvents,
    ];

    let first = evaluate_invariants(&snapshot, &specs);
    let second = evaluate_invariants(&snapshot, &specs);

    assert_eq!(first, second);
    assert_eq!(first.len(), 14);
    let ids = first
        .iter()
        .map(|result| result.invariant_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), first.len(), "invariant IDs must be unique");
    assert!(
        first
            .iter()
            .find(|result| result.invariant_id == "completion_check_passed:coverage-check")
            .is_some_and(|result| !result.passed)
    );
    assert!(
        first
            .iter()
            .find(|result| result.invariant_id == "completion_check_failed:coverage-check")
            .is_some_and(|result| result.passed)
    );
}

#[test]
fn execution_false_completion_detects_impossible_and_strict_coverage_cases_offline() {
    // Pins: completed is never accepted when MustNotComplete or strict independent coverage is unsatisfied.
    let completed = eval_snapshot(
        ExecutionRunStatus::Completed,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let strict_coverage = ExecutionInvariantSpec::MapCoverage {
        node_id: "research".to_string(),
        expected_keys: vec!["issuer-a".to_string(), "issuer-b".to_string()],
        require_all_when_completed: true,
    };
    let case = ExecutionEvalCaseResult::evaluate(
        "silent-incomplete-completed",
        &completed,
        &[strict_coverage],
        10,
    )
    .expect("case evaluation should succeed");
    assert!(case.impossible_case);
    assert!(case.execution_false_completion);
    assert!(!case.passed);

    let must_not_complete = ExecutionEvalCaseResult::evaluate(
        "declared-impossible-completed",
        &completed,
        &[ExecutionInvariantSpec::MustNotComplete],
        10,
    )
    .expect("case evaluation should succeed");
    assert!(must_not_complete.execution_false_completion);
    assert!(!must_not_complete.invariants[0].passed);
}

#[test]
fn execution_partial_with_missing_coverage_degrades_honestly_offline() {
    // Pins: incomplete independent coverage is an impossible-to-complete denominator without becoming an invariant failure when status is partial.
    let partial = eval_snapshot(
        ExecutionRunStatus::Partial,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    let case = ExecutionEvalCaseResult::evaluate(
        "silent-incomplete-partial",
        &partial,
        &[ExecutionInvariantSpec::MapCoverage {
            node_id: "research".to_string(),
            expected_keys: vec!["issuer-a".to_string(), "issuer-b".to_string()],
            require_all_when_completed: true,
        }],
        10,
    )
    .expect("case evaluation should succeed");

    assert!(case.impossible_case);
    assert!(!case.execution_false_completion);
    assert!(case.passed);
    assert!(case.invariants[0].passed);
    assert_eq!(case.invariants[0].completion_guard_passed, Some(false));
}

#[test]
fn execution_budget_progress_effect_and_authorization_invariants_fail_exactly_offline() {
    // Pins: budget escape, progress drift, duplicate effects, and capability escape are separate deterministic failures.
    let mut snapshot = eval_snapshot(
        ExecutionRunStatus::Partial,
        &[("issuer-a", ExecutionTaskStatus::Completed)],
    );
    snapshot.run.budget_ledger.overrun = true;
    snapshot.run.progress.completed_tasks = 0;
    let forbidden = CapabilityReference {
        name: "refund.issue".to_string(),
        version: "1".to_string(),
    };
    snapshot.harness.capability_calls = vec![
        ExecutionCapabilityCallObservation {
            logical_invocation_id: "logical-1".to_string(),
            reference: forbidden.clone(),
            item_key: Some("issuer-a".to_string()),
            replayed: false,
        },
        ExecutionCapabilityCallObservation {
            logical_invocation_id: "logical-1".to_string(),
            reference: forbidden,
            item_key: Some("issuer-a".to_string()),
            replayed: false,
        },
    ];
    let results = evaluate_invariants(
        &snapshot,
        &[
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::AllowedCapabilitiesOnly {
                references: vec![capability_ref()],
            },
        ],
    );

    assert_eq!(results.len(), 4);
    assert!(results.iter().all(|result| !result.passed));
    assert_eq!(
        results
            .iter()
            .map(|result| result.invariant_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "budget_within_approved",
            "progress_matches_tasks",
            "no_duplicate_logical_effects",
            "allowed_capabilities_only",
        ]
    );
}
