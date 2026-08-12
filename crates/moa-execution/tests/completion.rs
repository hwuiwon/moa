use std::collections::BTreeMap;

use chrono::{Duration, TimeZone, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionBudgetLimit, ExecutionCancelPolicy, ExecutionCitation, ExecutionDeliverable,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget,
    ExecutionUsage, ExecutionWaitExpiryAction, ExecutionWaitPolicy, MapTask, RetryPolicy,
};
use moa_execution::{
    budget::BudgetLedger,
    capability::{ExecutionEstimate, ExecutionHash},
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
    completion::{CompletionEvaluationRequest, CompletionStatus, evaluate_completion},
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskId, ExecutionTaskProjection,
        ExecutionTaskStatus,
    },
};
use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, FileFailurePersistence},
};
use serde_json::{Value, json};
use uuid::Uuid;

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn property_strict_subset_coverage_never_completes(
        expected_count in 1_usize..=24,
        retained_seed in 0_usize..24,
    ) {
        // Pins: any strict subset of declared map coverage must fail the completion gate.
        let retained_count = retained_seed.min(expected_count - 1);
        let expected = (0..expected_count)
            .map(|index| Value::String(format!("item-{index}")))
            .collect::<Vec<_>>();
        let (mut goal, mut plan, mut projection) = map_fixture();
        goal.requirements.clear();
        goal.completion_checks
            .retain(|check| matches!(check.kind, CompletionCheckKind::OutputSchema));
        goal.coverage[0].expected_items = Value::Array(expected.clone());
        goal.coverage[0].require_all = true;
        for node in &mut plan.definition.nodes {
            node.requirement_ids.clear();
        }
        let ExecutionOperation::Map { items, max_items, .. } =
            &mut plan.definition.nodes[0].operation
        else {
            unreachable!("map fixture must contain a map node");
        };
        *items = Value::Array(expected);
        *max_items = expected_count as u64;
        projection.tasks.retain(|task| task.node_id == "output");
        for index in 0..retained_count {
            projection.tasks.push(task(
                Uuid::from_u128(10_000 + index as u128),
                "inspect",
                &format!("string:\"item-{index}\""),
                completed(json!({ "ok": true }), vec![]),
            ));
        }

        let evaluation = evaluate_completion(request(
            goal,
            plan,
            projection,
            json!({ "ok": true }),
        ))
        .expect("strict-subset coverage evaluation");
        prop_assert_ne!(evaluation.status, CompletionStatus::Completed);
        prop_assert!(evaluation.gaps.iter().any(|gap| gap == "coverage expected failed"));
    }
}

fn property_config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/properties.txt",
        ))),
        ..ProptestConfig::default()
    }
}

#[test]
fn completion_refuses_missing_citations_and_invalid_deliverables() {
    // Pins: useful terminal output is partial, never completed, when evidence or deliverables fail.
    let (goal, plan, mut projection) = ordinary_fixture();
    let lookup = projection
        .tasks
        .iter_mut()
        .find(|task| task.node_id == "lookup")
        .expect("lookup task");
    lookup.outcome = Some(completed(json!({ "order": "ord-1" }), vec![]));

    let evaluation = evaluate_completion(request(goal, plan, projection, json!({ "result": 7 })))
        .expect("evaluate completion");
    assert_eq!(evaluation.status, CompletionStatus::Partial);
    assert!(
        evaluation
            .checks
            .iter()
            .any(|check| { check.check_id == "citations" && !check.passed })
    );
    assert!(
        evaluation
            .gaps
            .iter()
            .any(|gap| gap.contains("deliverable"))
    );
}

#[test]
fn completion_succeeds_only_when_all_declared_gates_pass() {
    // Pins: schema, requirement, deliverable, and per-task citation gates all pass together.
    let (goal, plan, projection) = ordinary_fixture();
    let evaluation =
        evaluate_completion(request(goal, plan, projection, json!({ "result": "ok" })))
            .expect("evaluate completion");
    assert_eq!(evaluation.status, CompletionStatus::Completed);
    assert!(evaluation.checks.iter().all(|check| check.passed));
    assert_eq!(evaluation.satisfied_requirement_ids, ["req_one"]);
    assert!(evaluation.unsatisfied_requirement_ids.is_empty());
    assert!(evaluation.gaps.is_empty());
}

#[test]
fn map_coverage_rejects_unexpected_extra_keys_even_when_require_all_is_false() {
    // Pins: optional expected coverage still rejects observed failures and unexpected keys.
    let (goal, plan, projection) = map_fixture();
    let evaluation = evaluate_completion(request(goal, plan, projection, json!({ "ok": true })))
        .expect("evaluate map coverage");
    assert_eq!(evaluation.status, CompletionStatus::Partial);
    let coverage = evaluation
        .checks
        .iter()
        .find(|check| check.check_id == "coverage")
        .expect("coverage check");
    assert!(!coverage.passed);
    assert!(
        coverage
            .evidence
            .to_string()
            .contains("string:\\\"extra\\\"")
    );
}

#[test]
fn multiple_coverage_contracts_for_one_map_all_gate_the_requirement() {
    // Pins: one passing coverage contract cannot hide another failed contract for the same map.
    let (mut goal, plan, mut projection) = map_fixture();
    projection
        .tasks
        .retain(|task| task.item_key != "string:\"extra\"");
    goal.coverage.push(CoverageRequirement {
        id: "missing".to_string(),
        description: "Missing item coverage".to_string(),
        map_node_id: "inspect".to_string(),
        expected_items: json!(["missing"]),
        require_all: true,
    });

    let evaluation = evaluate_completion(request(goal, plan, projection, json!({ "ok": true })))
        .expect("evaluate combined coverage contracts");
    assert!(evaluation.satisfied_requirement_ids.is_empty());
    assert_eq!(evaluation.unsatisfied_requirement_ids, ["req_one"]);
    assert!(
        evaluation
            .gaps
            .contains(&"coverage missing failed".to_string())
    );
}

#[test]
fn optional_empty_coverage_universe_passes_without_map_tasks() {
    // Pins: an optional empty universe is complete without inventing a task or observed item.
    let (mut goal, plan, mut projection) = map_fixture();
    goal.coverage[0].expected_items = json!([]);
    projection.tasks.retain(|task| task.node_id != "inspect");

    let evaluation = evaluate_completion(request(goal, plan, projection, json!({ "ok": true })))
        .expect("evaluate empty optional coverage");
    assert_eq!(evaluation.status, CompletionStatus::Completed);
    assert_eq!(evaluation.satisfied_requirement_ids, ["req_one"]);
    assert!(evaluation.unsatisfied_requirement_ids.is_empty());
}

#[test]
fn completion_always_validates_terminal_schemas_without_an_explicit_schema_check() {
    // Pins: terminal plan/output-node schemas gate Completed independently of CompletionCheckKind::OutputSchema.
    let (mut goal, mut plan, projection) = ordinary_fixture();
    goal.completion_checks
        .retain(|check| !matches!(check.kind, CompletionCheckKind::OutputSchema));
    let result_schema = json!({
        "type": "object",
        "required": ["result"],
        "properties": { "result": { "type": "string" } }
    });
    plan.definition.output_schema = result_schema.clone();
    plan.definition.nodes[1].output_schema = result_schema;

    let evaluation = evaluate_completion(request(goal, plan, projection, json!({ "result": 7 })))
        .expect("evaluate implicit terminal schemas");
    assert_eq!(evaluation.status, CompletionStatus::Partial);
    assert!(
        evaluation
            .gaps
            .contains(&"terminal output is missing or violates its declared schemas".to_string())
    );
}

#[test]
fn terminal_plan_and_output_node_schemas_must_both_pass() {
    // Pins: a terminal-node schema cannot mask an incompatible plan-level schema.
    let (mut goal, mut plan, projection) = ordinary_fixture();
    goal.completion_checks
        .retain(|check| !matches!(check.kind, CompletionCheckKind::OutputSchema));
    plan.definition.output_schema = json!({
        "type": "object",
        "required": ["result"],
        "properties": { "result": { "type": "integer" } }
    });
    plan.definition.nodes[1].output_schema = json!({ "type": "object" });

    let evaluation =
        evaluate_completion(request(goal, plan, projection, json!({ "result": "ok" })))
            .expect("evaluate independent terminal schemas");
    assert_eq!(evaluation.status, CompletionStatus::Partial);
    assert!(
        evaluation
            .gaps
            .contains(&"terminal output is missing or violates its declared schemas".to_string())
    );
}

#[test]
fn deadline_is_exceeded_only_after_the_exact_instant() {
    // Pins: equality remains admissible while any instant after the deadline degrades honestly.
    let (goal, plan, projection) = ordinary_fixture();
    let mut at_deadline = request(
        goal.clone(),
        plan.clone(),
        projection.clone(),
        json!({ "result": "ok" }),
    );
    at_deadline.budget_ledger.limit.deadline_at = Some(at_deadline.now);
    let evaluation = evaluate_completion(at_deadline).expect("evaluate exact deadline instant");
    assert_eq!(evaluation.status, CompletionStatus::Completed);

    let mut after_deadline = request(goal, plan, projection, json!({ "result": "ok" }));
    after_deadline.budget_ledger.limit.deadline_at =
        Some(after_deadline.now - Duration::milliseconds(1));
    let evaluation = evaluate_completion(after_deadline).expect("evaluate elapsed deadline");
    assert_eq!(evaluation.status, CompletionStatus::Partial);
    assert!(
        evaluation
            .gaps
            .contains(&"execution deadline exceeded".to_string())
    );
}

#[test]
fn completion_status_precedence_is_blocked_then_unsupported_then_budget() {
    // Pins: live waits outrank unsupported/budget, unsupported outranks budget, and useful budget exits are partial.
    let (goal, plan, mut projection) = ordinary_fixture();
    projection
        .node_statuses
        .insert("lookup".to_string(), ExecutionNodeStatus::Waiting);
    let lookup = projection
        .tasks
        .iter_mut()
        .find(|task| task.node_id == "lookup")
        .expect("lookup task");
    lookup.status = ExecutionTaskStatus::WaitingInput;
    lookup.outcome = Some(outcome(ExecutionTaskResult::NeedsInput {
        question: "Which order?".to_string(),
        audience: moa_artifacts::execution_plan::InputAudience::User,
    }));
    let mut blocked_request = request(goal, plan, projection, json!({ "result": "partial" }));
    blocked_request.budget_ledger.overrun = true;
    let blocked = evaluate_completion(blocked_request).expect("evaluate blocked precedence");
    assert_eq!(blocked.status, CompletionStatus::Blocked);

    let (goal, mut plan, mut projection) = ordinary_fixture();
    plan.definition.nodes[1].requirement_ids.clear();
    projection
        .node_statuses
        .insert("lookup".to_string(), ExecutionNodeStatus::Failed);
    let lookup = projection
        .tasks
        .iter_mut()
        .find(|task| task.node_id == "lookup")
        .expect("lookup task");
    lookup.status = ExecutionTaskStatus::Failed;
    lookup.outcome = Some(outcome(ExecutionTaskResult::Failed {
        class: moa_artifacts::execution_plan::ExecutionFailureClass::Unsupported,
        message: "unsupported source".to_string(),
    }));
    let mut unsupported_request = request(goal, plan, projection, json!({}));
    unsupported_request.terminal_output = None;
    unsupported_request.budget_ledger.overrun = true;
    let unsupported =
        evaluate_completion(unsupported_request).expect("evaluate unsupported precedence");
    assert_eq!(unsupported.status, CompletionStatus::Unsupported);

    let (goal, plan, projection) = ordinary_fixture();
    let mut partial_request = request(goal, plan, projection, json!({ "result": "ok" }));
    partial_request.budget_ledger.overrun = true;
    let partial = evaluate_completion(partial_request).expect("evaluate useful budget exit");
    assert_eq!(partial.status, CompletionStatus::Partial);

    let (goal, mut plan, mut projection) = ordinary_fixture();
    plan.definition.nodes[1].requirement_ids.clear();
    projection
        .node_statuses
        .insert("lookup".to_string(), ExecutionNodeStatus::Failed);
    let lookup = projection
        .tasks
        .iter_mut()
        .find(|task| task.node_id == "lookup")
        .expect("lookup task");
    lookup.status = ExecutionTaskStatus::Failed;
    lookup.outcome = Some(outcome(ExecutionTaskResult::Failed {
        class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
        message: "terminal failure".to_string(),
    }));
    let mut failed_request = request(goal, plan, projection, json!({}));
    failed_request.terminal_output = None;
    failed_request.budget_ledger.overrun = true;
    let failed = evaluate_completion(failed_request).expect("evaluate failed budget exit");
    assert_eq!(failed.status, CompletionStatus::Failed);
}

#[test]
fn skipped_declaring_nodes_are_neutral_when_another_path_completes() {
    // Pins: skipped serving paths neither satisfy nor fail a requirement when another declaring path completes.
    let (mut goal, mut plan, mut projection) = ordinary_fixture();
    goal.completion_checks
        .retain(|check| matches!(check.kind, CompletionCheckKind::OutputSchema));
    plan.definition.nodes[1].requirement_ids.clear();
    let mut alternate = plan.definition.nodes[0].clone();
    alternate.id = "alternate".to_string();
    plan.definition.nodes.push(alternate);
    projection
        .node_statuses
        .insert("lookup".to_string(), ExecutionNodeStatus::Skipped);
    projection
        .node_statuses
        .insert("alternate".to_string(), ExecutionNodeStatus::Completed);
    projection.tasks.retain(|task| task.node_id != "lookup");
    projection.tasks.push(task(
        Uuid::from_u128(3),
        "alternate",
        "",
        completed(json!({ "order": "ord-1" }), vec![]),
    ));

    let completed_evaluation = evaluate_completion(request(
        goal.clone(),
        plan.clone(),
        projection.clone(),
        json!({ "result": "ok" }),
    ))
    .expect("alternate path completion");
    assert_eq!(completed_evaluation.satisfied_requirement_ids, ["req_one"]);
    assert_eq!(completed_evaluation.status, CompletionStatus::Completed);

    projection
        .node_statuses
        .insert("alternate".to_string(), ExecutionNodeStatus::Skipped);
    projection.tasks.retain(|task| task.node_id != "alternate");
    let skipped_evaluation =
        evaluate_completion(request(goal, plan, projection, json!({ "result": "ok" })))
            .expect("all paths skipped");
    assert!(skipped_evaluation.satisfied_requirement_ids.is_empty());
    assert_eq!(skipped_evaluation.unsatisfied_requirement_ids, ["req_one"]);
}

fn ordinary_fixture() -> (
    ExecutionGoalContract,
    CanonicalExecutionPlan,
    ExecutionProjection,
) {
    let goal = ExecutionGoalContract {
        objective: "Return an evidenced order result".to_string(),
        requirements: vec![ExecutionRequirement {
            id: "req_one".to_string(),
            description: "Look up the order".to_string(),
        }],
        deliverables: vec![ExecutionDeliverable {
            id: "result".to_string(),
            description: "String result".to_string(),
            output_pointer: "/result".to_string(),
            schema: json!({ "type": "string" }),
        }],
        coverage: vec![],
        constraints: vec![],
        completion_checks: vec![
            CompletionCheck {
                id: "output_schema".to_string(),
                description: "Validate output".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::OutputSchema,
            },
            CompletionCheck {
                id: "citations".to_string(),
                description: "Require evidence".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::Citations {
                    node_ids: vec!["lookup".to_string()],
                    min_per_task: 1,
                },
            },
        ],
    };
    let plan = canonical(vec![
        node(
            "lookup",
            &[],
            ExecutionOperation::Capability {
                reference: capability(),
            },
        ),
        node(
            "output",
            &["lookup"],
            ExecutionOperation::Output {
                value: json!({ "$ref": "$.nodes.lookup.output" }),
            },
        ),
    ]);
    let run_uid = Uuid::from_u128(1);
    let projection = ExecutionProjection {
        plan_revision: 0,
        node_statuses: BTreeMap::from([
            ("lookup".to_string(), ExecutionNodeStatus::Completed),
            ("output".to_string(), ExecutionNodeStatus::Completed),
        ]),
        tasks: vec![
            task(
                run_uid,
                "lookup",
                "",
                completed(
                    json!({ "order": "ord-1" }),
                    vec![ExecutionCitation {
                        source_id: "order-db".to_string(),
                        uri: None,
                        locator: Some(json!({ "order_id": "ord-1" })),
                    }],
                ),
            ),
            task(
                run_uid,
                "output",
                "",
                completed(json!({ "result": "ok" }), vec![]),
            ),
        ],
    };
    (goal, plan, projection)
}

fn map_fixture() -> (
    ExecutionGoalContract,
    CanonicalExecutionPlan,
    ExecutionProjection,
) {
    let goal = ExecutionGoalContract {
        objective: "Cover expected items".to_string(),
        requirements: vec![ExecutionRequirement {
            id: "req_one".to_string(),
            description: "Inspect expected items".to_string(),
        }],
        deliverables: vec![],
        coverage: vec![CoverageRequirement {
            id: "expected".to_string(),
            description: "Expected item coverage".to_string(),
            map_node_id: "inspect".to_string(),
            expected_items: json!(["one"]),
            require_all: false,
        }],
        constraints: vec![],
        completion_checks: vec![
            CompletionCheck {
                id: "output_schema".to_string(),
                description: "Validate output".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::OutputSchema,
            },
            CompletionCheck {
                id: "coverage".to_string(),
                description: "Validate coverage".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::MapCoverage {
                    map_node_id: "inspect".to_string(),
                },
            },
        ],
    };
    let plan = canonical(vec![
        node(
            "inspect",
            &[],
            ExecutionOperation::Map {
                items: json!(["one", "extra"]),
                item_key: "".to_string(),
                max_items: 2,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability(),
                },
            },
        ),
        node(
            "output",
            &["inspect"],
            ExecutionOperation::Output {
                value: json!({ "ok": true }),
            },
        ),
    ]);
    let run_uid = Uuid::from_u128(2);
    let projection = ExecutionProjection {
        plan_revision: 0,
        node_statuses: BTreeMap::from([
            ("inspect".to_string(), ExecutionNodeStatus::Completed),
            ("output".to_string(), ExecutionNodeStatus::Completed),
        ]),
        tasks: vec![
            task(
                run_uid,
                "inspect",
                "string:\"one\"",
                completed(json!({ "ok": true }), vec![]),
            ),
            task(
                run_uid,
                "inspect",
                "string:\"extra\"",
                completed(json!({ "ok": true }), vec![]),
            ),
            task(
                run_uid,
                "output",
                "",
                completed(json!({ "ok": true }), vec![]),
            ),
        ],
    };
    (goal, plan, projection)
}

fn request(
    goal: ExecutionGoalContract,
    plan: CanonicalExecutionPlan,
    projection: ExecutionProjection,
    terminal_output: Value,
) -> CompletionEvaluationRequest {
    CompletionEvaluationRequest {
        goal,
        plan,
        run_input: json!({}),
        projection,
        terminal_output: Some(terminal_output),
        budget_ledger: BudgetLedger::new(ExecutionBudgetLimit {
            max_cost_microusd: Some(100),
            max_tokens: Some(100),
            max_tasks: Some(100),
            max_tool_calls: Some(100),
            max_retrieved_bytes: Some(100),
            deadline_at: Some(
                Utc.with_ymd_and_hms(2026, 7, 14, 0, 0, 0)
                    .single()
                    .expect("deadline"),
            ),
        }),
        now: Utc
            .with_ymd_and_hms(2026, 7, 13, 0, 0, 0)
            .single()
            .expect("time"),
    }
}

fn canonical(nodes: Vec<ExecutionNode>) -> CanonicalExecutionPlan {
    CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::After {
                    delay_seconds: 3_600,
                },
                on_expiry: ExecutionWaitExpiryAction::FailTask,
            },
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes,
        },
        plan_hash: ExecutionHash::from_bytes([1; 32]),
        catalog_hash: ExecutionHash::from_bytes([2; 32]),
        estimate: ExecutionEstimate::default(),
        report: ExecutionValidationReport::default(),
    }
}

fn node(id: &str, dependencies: &[&str], operation: ExecutionOperation) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req_one".to_string()],
        depends_on: dependencies.iter().map(|id| (*id).to_string()).collect(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation,
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        budget: None,
    }
}

fn task(
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    outcome: ExecutionTaskOutcome,
) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(run_uid, node_id, item_key).expect("task id"),
        node_id: node_id.to_string(),
        item_key: item_key.to_string(),
        status: ExecutionTaskStatus::Completed,
        attempt: 1,
        generation: 1,
        input: json!({}),
        outcome: Some(outcome),
    }
}

fn completed(output: Value, citations: Vec<ExecutionCitation>) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result: ExecutionTaskResult::Completed { output, citations },
    }
}

fn outcome(result: ExecutionTaskResult) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result,
    }
}

fn capability() -> CapabilityReference {
    CapabilityReference {
        name: "orders.lookup".to_string(),
        version: "v1".to_string(),
    }
}
