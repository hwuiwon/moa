use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionBudgetLimit, ExecutionFailureClass, ExecutionNode,
    ExecutionOperation, PlanAmendment, PlanAmendmentOperation, RetryPolicy,
};
use moa_core::config::ExecutionConfig;
use moa_execution::{
    capability::{
        ExecutionEstimate, ExecutionHash, amendment_hash, amendment_operations_fingerprint,
    },
    completion::CompletionStatus,
    replan::{
        ReplanDecision, ReplanEvaluationRequest, ReplanStopReason, evaluate_replan_stop,
        failure_fingerprint, replan_stop_gaps, replan_stop_status,
    },
    state::FailureFingerprintInput,
};
use serde_json::json;

#[test]
fn failure_fingerprint_normalizes_case_trim_and_ascii_whitespace() {
    // Pins: equivalent failure messages count toward the same deterministic loop threshold.
    let first = failure("  PROVIDER\tTimed   OUT\n");
    let second = failure("provider timed out");
    assert_eq!(
        failure_fingerprint(&first).expect("first fingerprint"),
        failure_fingerprint(&second).expect("second fingerprint")
    );
}

#[test]
fn amendment_operations_fingerprint_ignores_replay_and_prose_fields() {
    // Pins: loop identity is operation semantics, while the full amendment hash remains exact
    // revision-scoped repository replay identity.
    let first = request().amendment;
    let mut same_operations = first.clone();
    same_operations.base_plan_revision += 1;
    same_operations.reason = "Different planner prose".to_string();
    same_operations.evidence = json!({"different": "evidence"});

    assert_ne!(
        amendment_hash(&first).expect("hash first exact amendment"),
        amendment_hash(&same_operations).expect("hash changed exact amendment"),
        "full amendment identity must retain revision and prose for exact replay"
    );
    assert_eq!(
        amendment_operations_fingerprint(&first).expect("fingerprint first operations"),
        amendment_operations_fingerprint(&same_operations)
            .expect("fingerprint semantically identical operations"),
        "base revision, reason, and evidence must not evade the loop guard"
    );

    let PlanAmendmentOperation::AddNode { node } = &mut same_operations.operations[0] else {
        panic!("fixture should contain one add-node operation");
    };
    node.input = json!({"semantic": "change"});
    assert_ne!(
        amendment_operations_fingerprint(&first).expect("fingerprint original operations"),
        amendment_operations_fingerprint(&same_operations).expect("fingerprint changed operations"),
        "operation semantics must remain part of loop identity"
    );
}

#[test]
fn replan_stops_on_repeated_operations_without_exact_amendment_replay() {
    // Pins: a later-revision/prose variant of a persisted operation set reaches the typed
    // DuplicateAmendment stop instead of being accepted as fresh work.
    let mut evaluation = request();
    let mut prior = evaluation.amendment.clone();
    prior.base_plan_revision -= 1;
    prior.reason = "Earlier planner prose".to_string();
    prior.evidence = json!({"earlier": true});
    assert_ne!(
        amendment_hash(&prior).expect("hash prior exact amendment"),
        amendment_hash(&evaluation.amendment).expect("hash proposed exact amendment")
    );
    evaluation.seen_amendment_fingerprints.insert(
        amendment_operations_fingerprint(&prior).expect("fingerprint persisted operations"),
    );

    assert_eq!(
        evaluate_replan_stop(evaluation),
        ReplanDecision::Stop {
            reason: ReplanStopReason::DuplicateAmendment
        }
    );
}

#[test]
fn replan_stop_status_and_gaps_are_stable_and_bounded() {
    // Pins: every stop path uses one useful-work status rule and emits an exact typed reason gap
    // while retaining only bounded diagnostic detail.
    assert_eq!(replan_stop_status(false, 0), CompletionStatus::Blocked);
    assert_eq!(replan_stop_status(true, 0), CompletionStatus::Partial);
    assert_eq!(replan_stop_status(false, 1), CompletionStatus::Partial);

    let detail = "x".repeat(600);
    assert_eq!(
        replan_stop_gaps(ReplanStopReason::DuplicateAmendment, Some(&detail)),
        vec![
            "replan stop reason: duplicate_amendment".to_string(),
            format!("replan stopped: {}", "x".repeat(512)),
        ]
    );
    for (reason, expected) in [
        (ReplanStopReason::DuplicatePlan, "duplicate_plan"),
        (ReplanStopReason::DuplicateAmendment, "duplicate_amendment"),
        (ReplanStopReason::RepeatedFailure, "repeated_failure"),
        (ReplanStopReason::NoProgress, "no_progress"),
        (ReplanStopReason::DeadlineExceeded, "deadline_exceeded"),
        (ReplanStopReason::BudgetExhausted, "budget_exhausted"),
    ] {
        assert_eq!(
            replan_stop_gaps(reason, None),
            vec![format!("replan stop reason: {expected}")]
        );
    }
}

#[test]
fn replan_stop_precedence_remains_deadline_budget_plan_amendment_failure_progress() {
    // Pins: adding a pre-validation loop seam cannot reorder the complete stop contract.
    let mut evaluation = request();
    evaluation.now = evaluation
        .remaining_budget
        .deadline_at
        .expect("fixture deadline")
        + chrono::Duration::seconds(1);
    evaluation.remaining_budget.max_tasks = Some(0);
    evaluation
        .seen_plan_hashes
        .insert(evaluation.proposed_plan_hash);
    evaluation
        .seen_amendment_fingerprints
        .insert(evaluation.proposed_amendment_fingerprint);
    let failure = failure("provider timed out");
    evaluation.failure_fingerprint_counts.insert(
        failure_fingerprint(&failure).expect("fingerprint failure"),
        2,
    );
    evaluation.current_failure = Some(failure);
    evaluation.amendment.operations = vec![PlanAmendmentOperation::RemovePendingNode {
        node_id: "pending".to_string(),
    }];

    assert_stop_reason(&evaluation, ReplanStopReason::DeadlineExceeded);
    evaluation.now = Utc
        .with_ymd_and_hms(2026, 7, 13, 12, 0, 0)
        .single()
        .expect("pre-deadline time");
    assert_stop_reason(&evaluation, ReplanStopReason::BudgetExhausted);
    evaluation.remaining_budget.max_tasks = Some(10);
    assert_stop_reason(&evaluation, ReplanStopReason::DuplicatePlan);
    evaluation.seen_plan_hashes.clear();
    assert_stop_reason(&evaluation, ReplanStopReason::DuplicateAmendment);
    evaluation.seen_amendment_fingerprints.clear();
    assert_stop_reason(&evaluation, ReplanStopReason::RepeatedFailure);
    evaluation.failure_fingerprint_counts.clear();
    evaluation.current_failure = None;
    assert_stop_reason(&evaluation, ReplanStopReason::NoProgress);
}

#[test]
fn replan_stops_at_repeated_failure_limit_without_revision_cap() {
    // Pins: only the fingerprint threshold stops repetition; there is no amendment-count field.
    let current = failure("provider timed out");
    let fingerprint = failure_fingerprint(&current).expect("fingerprint");
    let mut request = request();
    request.current_failure = Some(current);
    request.failure_fingerprint_counts.insert(fingerprint, 2);

    assert_eq!(
        evaluate_replan_stop(request),
        ReplanDecision::Stop {
            reason: ReplanStopReason::RepeatedFailure
        }
    );
}

#[test]
fn replan_rejects_duplicate_hashes_remove_only_and_exhausted_budget() {
    // Pins: duplicate hashes, no-progress patches, and resource exhaustion are independent stops.
    let mut duplicate = request();
    duplicate
        .seen_plan_hashes
        .insert(duplicate.proposed_plan_hash);
    assert_eq!(
        evaluate_replan_stop(duplicate),
        ReplanDecision::Stop {
            reason: ReplanStopReason::DuplicatePlan
        }
    );

    let mut remove_only = request();
    remove_only.amendment.operations = vec![PlanAmendmentOperation::RemovePendingNode {
        node_id: "pending".to_string(),
    }];
    remove_only.proposed_amendment_fingerprint =
        amendment_operations_fingerprint(&remove_only.amendment)
            .expect("fingerprint remove-only amendment");
    assert_eq!(
        evaluate_replan_stop(remove_only),
        ReplanDecision::Stop {
            reason: ReplanStopReason::NoProgress
        }
    );

    let mut exhausted = request();
    exhausted.remaining_budget.max_tasks = Some(0);
    assert_eq!(
        evaluate_replan_stop(exhausted),
        ReplanDecision::Stop {
            reason: ReplanStopReason::BudgetExhausted
        }
    );
}

fn request() -> ReplanEvaluationRequest {
    let amendment = PlanAmendment {
        schema_version: 1,
        base_plan_revision: 8,
        reason: "Try a replacement path".to_string(),
        evidence: json!({ "failure": "provider" }),
        operations: vec![PlanAmendmentOperation::AddNode {
            node: ExecutionNode {
                id: "replacement".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                depends_on: vec!["completed".to_string()],
                when: None,
                input: json!({}),
                output_schema: json!({ "type": "object" }),
                operation: ExecutionOperation::Capability {
                    reference: CapabilityReference {
                        name: "orders.backup".to_string(),
                        version: "v1".to_string(),
                    },
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            },
        }],
    };
    ReplanEvaluationRequest {
        now: Utc
            .with_ymd_and_hms(2026, 7, 13, 12, 0, 0)
            .single()
            .expect("time"),
        remaining_budget: ExecutionBudgetLimit {
            max_cost_microusd: Some(100),
            max_tokens: Some(100),
            max_tasks: Some(10),
            max_tool_calls: Some(100),
            max_retrieved_bytes: Some(100),
            deadline_at: Some(
                Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0)
                    .single()
                    .expect("deadline"),
            ),
        },
        proposed_estimate: ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        },
        proposed_plan_hash: ExecutionHash::from_bytes([1; 32]),
        proposed_amendment_fingerprint: amendment_operations_fingerprint(&amendment)
            .expect("fingerprint amendment operations"),
        seen_plan_hashes: BTreeSet::new(),
        seen_amendment_fingerprints: BTreeSet::new(),
        failure_fingerprint_counts: BTreeMap::new(),
        current_failure: None,
        unresolved_requirement_ids: BTreeSet::from(["req_one".to_string()]),
        amendment,
        config: ExecutionConfig::default(),
    }
}

fn failure(message: &str) -> FailureFingerprintInput {
    FailureFingerprintInput {
        class: ExecutionFailureClass::Retryable,
        node_id: "lookup".to_string(),
        capability_ref: Some(CapabilityReference {
            name: "orders.lookup".to_string(),
            version: "v1".to_string(),
        }),
        message: message.to_string(),
    }
}

fn assert_stop_reason(request: &ReplanEvaluationRequest, reason: ReplanStopReason) {
    assert_eq!(
        evaluate_replan_stop(request.clone()),
        ReplanDecision::Stop { reason }
    );
}
