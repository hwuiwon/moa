use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit,
    ExecutionCondition, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionReference, ExecutionRequirement, ExecutionTaskOutcome,
    ExecutionTaskResult, ExecutionUsage, MapTask, RetryPolicy,
};
use moa_config::ExecutionConfig;
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    tools::IdempotencyClass,
};
use moa_execution::{
    budget::BudgetLedger,
    capability::{
        CapabilityPolicyContext, CapabilitySource, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionClass, ExecutionEstimate, ExecutionHash, catalog_hash,
    },
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
    completion::{CompletionEvaluationRequest, CompletionStatus, evaluate_completion},
    interpreter::{ScheduleRequest, schedule as schedule_outcome},
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskId, ExecutionTaskProjection,
        ExecutionTaskStatus, LogicalTaskKind, ScheduleDecision, TerminalProjection,
        input_resume_counters, retry_dispatch_counters, supersede_waiting_replan,
        task_status_from_outcome, validate_outcome_generation,
    },
};
use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, FileFailurePersistence},
};
use serde_json::{Value, json};
use uuid::Uuid;

fn schedule(request: ScheduleRequest) -> Result<ScheduleDecision, moa_execution::Error> {
    schedule_outcome(request).map(|outcome| outcome.decision)
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn property_scheduler_is_idempotent_for_unchanged_projection(
        item_count in 0_u64..=12,
        completed_seed in 0_u64..=12,
        run_seed in 1_u128..=u128::MAX,
    ) {
        // Pins: replaying an unchanged durable projection yields the exact same decision and projection.
        let completed_count = completed_seed.min(item_count);
        let items = (0..item_count).map(|item| json!(item)).collect::<Vec<_>>();
        let map = node(
            "inspect",
            &[],
            ExecutionOperation::Map {
                items: Value::Array(items),
                item_key: "".to_string(),
                max_items: item_count,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability(),
                },
            },
        );
        let plan = canonical(vec![map, output_node("inspect")]);
        let run_uid = Uuid::from_u128(run_seed);
        let statuses = if completed_count == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([("inspect".to_string(), ExecutionNodeStatus::Running)])
        };
        let tasks = (0..completed_count)
            .map(|item| {
                completed_item_task(
                    run_uid,
                    "inspect",
                    &format!("number:{item}"),
                    json!({ "item": item }),
                    json!({ "ok": true }),
                )
            })
            .collect::<Vec<_>>();
        let request = request(run_uid, plan, statuses, tasks);

        let first = schedule_outcome(request.clone()).expect("generated projection schedules");
        let second = schedule_outcome(request).expect("unchanged generated projection schedules");
        prop_assert_eq!(first, second);
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
fn scheduler_materializes_every_ready_map_item_with_stable_typed_keys() {
    // Pins: max_items is accounting, not a hidden active-worker cap.
    let run_uid = Uuid::from_u128(11);
    let plan = canonical(vec![
        ExecutionNode {
            id: "inspect".to_string(),
            requirement_ids: vec!["req_one".to_string()],
            depends_on: vec![],
            when: None,
            input: json!({ "item": { "$item": true }, "key": { "$item_key": true } }),
            output_schema: json!({ "type": "object" }),
            operation: ExecutionOperation::Map {
                items: json!([1, "1", { "id": 1 }]),
                item_key: "".to_string(),
                max_items: 3,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability(),
                },
            },
            retry: retry(),
            budget: None,
        },
        output_node("inspect"),
    ]);

    let decision = schedule(request(run_uid, plan, BTreeMap::new(), vec![])).expect("schedule map");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected ready tasks, got {decision:?}");
    };
    assert_eq!(tasks.len(), 3);
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>(),
        ["number:1", "object:{\"id\":1}", "string:\"1\""]
    );
    for task in tasks {
        assert_eq!(
            task.task_id,
            ExecutionTaskId::derive(run_uid, "inspect", &task.item_key).expect("stable id")
        );
        assert_eq!(task.generation, 1);
        assert_eq!(
            task.reservation,
            ExecutionEstimate {
                cost_microusd: 7,
                tokens: 11,
                tool_calls: 3,
                retrieved_bytes: 13,
                tasks: 1,
            }
        );
    }
}

#[test]
fn scheduler_rejects_duplicate_dynamic_map_keys() {
    // Pins: duplicate item identities fail materialization before any task can be returned.
    let plan = canonical(vec![
        ExecutionNode {
            id: "inspect".to_string(),
            requirement_ids: vec!["req_one".to_string()],
            depends_on: vec![],
            when: None,
            input: json!({ "$item": true }),
            output_schema: json!({ "type": "object" }),
            operation: ExecutionOperation::Map {
                items: json!([{ "id": "same" }, { "id": "same" }]),
                item_key: "/id".to_string(),
                max_items: 2,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability(),
                },
            },
            retry: retry(),
            budget: None,
        },
        output_node("inspect"),
    ]);
    assert!(schedule(request(Uuid::from_u128(12), plan, BTreeMap::new(), vec![])).is_err());
}

#[test]
fn scheduler_does_not_validate_a_partial_map_as_its_terminal_aggregate() {
    // Pins: an in-flight map is not validated against its completed aggregate schema.
    let run_uid = Uuid::from_u128(120);
    let mut map = node(
        "inspect",
        &[],
        ExecutionOperation::Map {
            items: json!(["one", "two"]),
            item_key: "".to_string(),
            max_items: 2,
            item_output_schema: json!({ "type": "object" }),
            task: MapTask::Capability {
                reference: capability(),
            },
        },
    );
    map.output_schema = json!({
        "type": "object",
        "required": ["items"],
        "properties": { "items": { "type": "array", "minItems": 2 } }
    });
    let plan = canonical(vec![map, output_node("inspect")]);
    let statuses = BTreeMap::from([("inspect".to_string(), ExecutionNodeStatus::Running)]);
    let tasks = vec![completed_item_task(
        run_uid,
        "inspect",
        "string:\"one\"",
        json!({}),
        json!({ "ok": true }),
    )];

    let decision = schedule(request(run_uid, plan, statuses, tasks))
        .expect("partial map must not be validated as a terminal aggregate");
    assert_eq!(
        decision,
        ScheduleDecision::Waiting(vec![moa_execution::state::WaitingReason::Dependencies {
            node_ids: vec!["output".to_string()]
        }])
    );
}

#[test]
fn scheduler_builds_exact_hierarchical_reducer_batch_inputs() {
    // Pins: reducer tasks use r{round}:b{batch} keys and exact structured batch input.
    let mut reduce = node(
        "reduce",
        &[],
        ExecutionOperation::Reduce {
            items: json!([1, 2, 3, 4, 5]),
            max_items: 5,
            reducer: moa_artifacts::execution_plan::ExecutionReducer::Capability {
                reference: capability(),
            },
            batch_size: 2,
        },
    );
    reduce.output_schema = json!({});
    let plan = canonical(vec![reduce, output_node("reduce")]);
    let decision = schedule(request(Uuid::from_u128(13), plan, BTreeMap::new(), vec![]))
        .expect("schedule reduce");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected reducer tasks, got {decision:?}");
    };
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>(),
        ["r1:b0", "r1:b1", "r1:b2"]
    );
    assert_eq!(
        tasks[0].input,
        json!({ "round": 1, "batch_index": 0, "items": [1, 2] })
    );
}

#[test]
fn scheduler_propagates_fixed_dependency_failed_terminal() {
    // Pins: terminal predecessor failure uses dependency_failed without configurable policy.
    let plan = canonical(vec![node(
        "output",
        &["lookup"],
        ExecutionOperation::Output { value: json!({}) },
    )]);
    let statuses = BTreeMap::from([
        ("lookup".to_string(), ExecutionNodeStatus::Failed),
        ("output".to_string(), ExecutionNodeStatus::Pending),
    ]);
    let decision = schedule(request(Uuid::from_u128(14), plan, statuses, vec![]))
        .expect("schedule dependency failure");
    let ScheduleDecision::Terminal(TerminalProjection::Failed { failure }) = decision else {
        panic!("expected failed terminal, got {decision:?}");
    };
    assert_eq!(
        failure.class,
        moa_artifacts::execution_plan::ExecutionFailureClass::DependencyFailed
    );
}

#[test]
fn scheduler_materializes_completion_verifier_after_ordinary_nodes_finish() {
    // Pins: verifier is one synthetic stable task with summaries but no embedded raw outputs.
    let run_uid = Uuid::from_u128(15);
    let plan = canonical(vec![
        node(
            "lookup",
            &[],
            ExecutionOperation::Capability {
                reference: capability(),
            },
        ),
        output_node("lookup"),
    ]);
    let statuses = BTreeMap::from([
        ("lookup".to_string(), ExecutionNodeStatus::Completed),
        ("output".to_string(), ExecutionNodeStatus::Completed),
    ]);
    let tasks = vec![
        completed_task(run_uid, "lookup", json!({ "secret": "raw" })),
        completed_task(run_uid, "output", json!({ "ok": true })),
    ];
    let decision = schedule(request(run_uid, plan, statuses, tasks)).expect("schedule verifier");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected verifier task, got {decision:?}");
    };
    assert_eq!(tasks.len(), 1);
    let verifier = &tasks[0];
    assert_eq!(verifier.node_id, "@check/semantic");
    assert_eq!(verifier.item_key, "check:semantic");
    assert_eq!(verifier.retry.max_attempts, 1);
    assert_eq!(
        verifier.reservation,
        ExecutionEstimate {
            cost_microusd: 400_000,
            tokens: 32_000,
            tool_calls: 8,
            retrieved_bytes: 2_000_000,
            tasks: 1,
        }
    );
    assert!(matches!(
        verifier.kind,
        LogicalTaskKind::CompletionVerifier { .. }
    ));
    let object = verifier
        .input
        .as_object()
        .expect("verifier input should be an object");
    assert_eq!(
        object.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "check_id",
            "description",
            "goal",
            "task_summaries",
            "terminal_output"
        ])
    );
    assert_eq!(object.get("check_id"), Some(&json!("semantic")));
    assert_eq!(object.get("terminal_output"), Some(&json!({ "ok": true })));
    let summaries = object
        .get("task_summaries")
        .and_then(Value::as_array)
        .expect("verifier summaries should be an array");
    assert_eq!(summaries.len(), 2);
    assert!(
        summaries
            .iter()
            .all(|summary| summary.get("output_hash").is_some())
    );
    assert!(!verifier.input.to_string().contains("secret"));
}

#[test]
fn scheduler_rejects_catalog_drift_and_validates_capability_input() {
    // Pins: a run revision uses only the catalog snapshot whose canonical hash is pinned by the plan.
    let run_uid = Uuid::from_u128(16);
    let mut capability_node = node(
        "lookup",
        &[],
        ExecutionOperation::Capability {
            reference: capability(),
        },
    );
    capability_node.input = json!({ "order_id": "ord-1" });
    capability_node.retry.max_attempts = 2;
    let plan = canonical(vec![capability_node, output_node("lookup")]);
    let decision = schedule(request(run_uid, plan.clone(), BTreeMap::new(), vec![]))
        .expect("matching catalog should schedule");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected ready capability task, got {decision:?}");
    };
    assert_eq!(
        tasks[0].reservation,
        ExecutionEstimate {
            cost_microusd: 14,
            tokens: 22,
            tool_calls: 6,
            retrieved_bytes: 26,
            tasks: 1,
        }
    );

    let mut drifted = request(run_uid, plan.clone(), BTreeMap::new(), vec![]);
    drifted.catalog.capabilities[0].estimate.tokens = 12;
    drifted.catalog.catalog_hash = catalog_hash(
        drifted.catalog.schema_version,
        &drifted.catalog.capabilities,
    )
    .expect("drifted catalog should hash");
    let error = schedule(drifted).expect_err("catalog content drift must be rejected");
    assert_eq!(
        error.to_string(),
        "invalid execution projection: scheduler capability catalog hash does not match the canonical plan"
    );

    let mut invalid_input = request(run_uid, plan.clone(), BTreeMap::new(), vec![]);
    invalid_input.catalog.capabilities[0].input_schema = json!({
        "type": "object",
        "required": ["customer_id"]
    });
    invalid_input.catalog.catalog_hash = catalog_hash(
        invalid_input.catalog.schema_version,
        &invalid_input.catalog.capabilities,
    )
    .expect("catalog should hash");
    invalid_input.plan.catalog_hash = invalid_input.catalog.catalog_hash;
    let error = schedule(invalid_input).expect_err("resolved capability input must validate");
    assert!(matches!(error, moa_execution::Error::Schema { .. }));

    let mut invalid_task_count = request(run_uid, plan, BTreeMap::new(), vec![]);
    invalid_task_count.catalog.capabilities[0].estimate.tasks = 2;
    invalid_task_count.catalog.catalog_hash = catalog_hash(
        invalid_task_count.catalog.schema_version,
        &invalid_task_count.catalog.capabilities,
    )
    .expect("catalog should hash");
    invalid_task_count.plan.catalog_hash = invalid_task_count.catalog.catalog_hash;
    let error = schedule(invalid_task_count)
        .expect_err("catalog capability estimates must reserve one task");
    assert!(error.to_string().contains("exactly one logical task"));
}

#[test]
fn scheduler_skips_false_condition_and_resolves_downstream_null() {
    // Pins: a false condition is an effective skipped node with JSON null output.
    let mut conditional = node(
        "conditional",
        &[],
        ExecutionOperation::Capability {
            reference: capability(),
        },
    );
    conditional.when = Some(ExecutionCondition::Equals {
        reference: ExecutionReference {
            path: "$.input.run".to_string(),
        },
        value: json!(true),
    });
    let mut output = output_node("conditional");
    output.output_schema = json!({});
    let mut plan = canonical(vec![conditional, output]);
    plan.definition.output_schema = json!({});
    let mut request = request(Uuid::from_u128(17), plan, BTreeMap::new(), vec![]);
    request.run_input = json!({ "run": false });

    let decision = schedule(request).expect("false condition should be deterministic");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected only the downstream output task, got {decision:?}");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].node_id, "output");
    assert!(matches!(
        &tasks[0].kind,
        LogicalTaskKind::Output { value } if value.is_null()
    ));
}

#[test]
fn scheduler_advances_every_hierarchical_reducer_round_to_final_output() {
    // Pins: completed reducer batches feed deterministic subsequent rounds until one output remains.
    let run_uid = Uuid::from_u128(18);
    let mut reduce = node(
        "reduce",
        &[],
        ExecutionOperation::Reduce {
            items: json!([1, 2, 3, 4, 5]),
            max_items: 5,
            reducer: moa_artifacts::execution_plan::ExecutionReducer::Capability {
                reference: capability(),
            },
            batch_size: 2,
        },
    );
    reduce.output_schema = json!({});
    let mut output = output_node("reduce");
    output.output_schema = json!({});
    let mut plan = canonical(vec![reduce, output]);
    plan.definition.output_schema = json!({});
    let statuses = BTreeMap::from([
        ("reduce".to_string(), ExecutionNodeStatus::Pending),
        ("output".to_string(), ExecutionNodeStatus::Pending),
    ]);
    let mut completed = vec![
        completed_item_task(run_uid, "reduce", "r1:b0", json!({}), json!(3)),
        completed_item_task(run_uid, "reduce", "r1:b1", json!({}), json!(7)),
        completed_item_task(run_uid, "reduce", "r1:b2", json!({}), json!(5)),
    ];

    let decision = schedule(request(
        run_uid,
        plan.clone(),
        statuses.clone(),
        completed.clone(),
    ))
    .expect("second reducer round should schedule");
    let ScheduleDecision::Ready(round_two) = decision else {
        panic!("expected second reducer round, got {decision:?}");
    };
    assert_eq!(round_two.len(), 2);
    assert_eq!(round_two[0].item_key, "r2:b0");
    assert_eq!(
        round_two[0].input,
        json!({ "round": 2, "batch_index": 0, "items": [3, 7] })
    );
    assert_eq!(
        round_two[1].input,
        json!({ "round": 2, "batch_index": 1, "items": [5] })
    );
    completed.push(completed_item_task(
        run_uid,
        "reduce",
        "r2:b0",
        round_two[0].input.clone(),
        json!(10),
    ));
    completed.push(completed_item_task(
        run_uid,
        "reduce",
        "r2:b1",
        round_two[1].input.clone(),
        json!(5),
    ));

    let decision = schedule(request(
        run_uid,
        plan.clone(),
        statuses.clone(),
        completed.clone(),
    ))
    .expect("final reducer round should schedule");
    let ScheduleDecision::Ready(round_three) = decision else {
        panic!("expected final reducer round, got {decision:?}");
    };
    assert_eq!(round_three.len(), 1);
    assert_eq!(round_three[0].item_key, "r3:b0");
    assert_eq!(
        round_three[0].input,
        json!({ "round": 3, "batch_index": 0, "items": [10, 5] })
    );
    completed.push(completed_item_task(
        run_uid,
        "reduce",
        "r3:b0",
        round_three[0].input.clone(),
        json!(15),
    ));

    let decision = schedule(request(run_uid, plan, statuses, completed))
        .expect("completed hierarchy should unlock output");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected terminal output task, got {decision:?}");
    };
    assert_eq!(tasks.len(), 1);
    assert!(matches!(
        &tasks[0].kind,
        LogicalTaskKind::Output { value } if value == &json!(15)
    ));
}

#[test]
fn scheduler_returns_the_effective_projection_used_for_terminal_completion() {
    // Pins: callers finalize against the same derived aggregate statuses that selected terminal.
    let run_uid = Uuid::from_u128(181);
    let mut map = node(
        "inspect",
        &[],
        ExecutionOperation::Map {
            items: json!([1]),
            item_key: "".to_string(),
            max_items: 1,
            item_output_schema: json!({ "type": "object" }),
            task: MapTask::Capability {
                reference: capability(),
            },
        },
    );
    map.output_schema = json!({
        "type": "object",
        "required": ["items"],
        "properties": {
            "items": {
                "type": "array",
                "minItems": 1,
                "maxItems": 1
            }
        }
    });
    let plan = canonical(vec![map, output_node("inspect")]);
    let statuses = BTreeMap::from([
        ("inspect".to_string(), ExecutionNodeStatus::Pending),
        ("output".to_string(), ExecutionNodeStatus::Completed),
    ]);
    let tasks = vec![
        completed_item_task(
            run_uid,
            "inspect",
            "number:1",
            json!({}),
            json!({ "ok": true }),
        ),
        completed_task(run_uid, "output", json!({ "ok": true })),
    ];
    let mut request = request(run_uid, plan, statuses, tasks);
    request
        .goal
        .completion_checks
        .retain(|check| matches!(check.kind, CompletionCheckKind::OutputSchema));

    let outcome = schedule_outcome(request.clone()).expect("schedule completed map run");
    let ScheduleDecision::Terminal(TerminalProjection::Completed { output }) = &outcome.decision
    else {
        panic!("expected completed terminal, got {:?}", outcome.decision);
    };
    assert_eq!(
        outcome.effective_projection.node_statuses.get("inspect"),
        Some(&ExecutionNodeStatus::Completed)
    );
    let evaluation = evaluate_completion(CompletionEvaluationRequest {
        goal: request.goal,
        plan: request.plan,
        run_input: request.run_input,
        projection: outcome.effective_projection,
        terminal_output: Some(output.clone()),
        budget_ledger: request.budget_ledger,
        now: request.now,
    })
    .expect("evaluate the scheduler's effective terminal projection");
    assert_eq!(evaluation.status, CompletionStatus::Completed);
}

#[test]
fn scheduler_returns_no_progress_for_unbacked_nonterminal_state() {
    // Pins: unfinished state with no runnable or durably waiting task is surfaced as NoProgress.
    let plan = canonical(vec![node(
        "lookup",
        &[],
        ExecutionOperation::Capability {
            reference: capability(),
        },
    )]);
    let statuses = BTreeMap::from([("lookup".to_string(), ExecutionNodeStatus::Running)]);

    let decision = schedule(request(Uuid::from_u128(121), plan, statuses, vec![]))
        .expect("unbacked running state should remain inspectable");
    assert_eq!(
        decision,
        ScheduleDecision::NoProgress {
            pending_node_ids: vec!["lookup".to_string()]
        }
    );
}

#[test]
fn task_transition_helpers_pin_retry_resume_generation_and_replan_supersession() {
    // Pins: durable redispatch counters and WaitingReplan cancellation follow the exact state machine.
    assert_eq!(
        retry_dispatch_counters(2, 7).expect("retry counters"),
        (3, 8)
    );
    assert_eq!(
        input_resume_counters(2, 7).expect("resume counters"),
        (2, 8)
    );
    validate_outcome_generation(8, 8).expect("current generation should persist");
    assert!(validate_outcome_generation(8, 7).is_err());

    let usage = ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    };
    let outcome = |result| ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage.clone(),
        result,
    };
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::NeedsInput {
                question: "Which order?".to_string(),
                audience: moa_artifacts::execution_plan::InputAudience::User,
            }),
            false,
        ),
        ExecutionTaskStatus::WaitingInput
    );
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::NeedsReplan {
                reason: "unsupported".to_string(),
                evidence: json!({}),
            }),
            false,
        ),
        ExecutionTaskStatus::WaitingReplan
    );
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                message: "retry".to_string(),
            }),
            true,
        ),
        ExecutionTaskStatus::Running
    );
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                message: "exhausted".to_string(),
            }),
            false,
        ),
        ExecutionTaskStatus::Failed
    );
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::Completed {
                output: json!({}),
                citations: vec![],
            }),
            false,
        ),
        ExecutionTaskStatus::Completed
    );
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::Cancelled {
                reason: "cancelled".to_string(),
            }),
            false,
        ),
        ExecutionTaskStatus::Cancelled
    );

    let run_uid = Uuid::from_u128(19);
    let waiting = ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(run_uid, "lookup", "").expect("task id"),
        node_id: "lookup".to_string(),
        item_key: String::new(),
        status: ExecutionTaskStatus::WaitingReplan,
        attempt: 2,
        generation: 3,
        input: json!({}),
        outcome: Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 5,
                tokens: 6,
                tool_calls: 1,
                retrieved_bytes: 7,
            },
            result: ExecutionTaskResult::NeedsReplan {
                reason: "provider removed operation".to_string(),
                evidence: json!({ "provider": "orders" }),
            },
        }),
    };
    let superseded = supersede_waiting_replan(&waiting).expect("supersede origin task");
    assert_eq!(superseded.task_id, waiting.task_id);
    assert_eq!(superseded.attempt, 2);
    assert_eq!(superseded.generation, 3);
    assert_eq!(superseded.status, ExecutionTaskStatus::Cancelled);
    assert_eq!(
        superseded.outcome.as_ref().map(|outcome| &outcome.usage),
        waiting.outcome.as_ref().map(|outcome| &outcome.usage)
    );
    assert!(matches!(
        superseded.outcome.map(|outcome| outcome.result),
        Some(ExecutionTaskResult::Cancelled { reason })
            if reason == "superseded_by_plan_revision"
    ));
}

#[test]
fn scheduler_ignores_cancelled_tasks_from_superseded_plan_revisions() {
    // Pins: amendment history keeps cancelled task rows whose nodes are absent from the active
    // plan, while the scheduler advances only the replacement branch.
    let run_uid = Uuid::from_u128(20);
    let replacement = node(
        "replacement",
        &[],
        ExecutionOperation::Capability {
            reference: capability(),
        },
    );
    let plan = canonical(vec![replacement, output_node("replacement")]);
    let superseded = ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(run_uid, "superseded", "").expect("task id"),
        node_id: "superseded".to_string(),
        item_key: String::new(),
        status: ExecutionTaskStatus::Cancelled,
        attempt: 1,
        generation: 1,
        input: json!({}),
        outcome: Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 0,
                tokens: 0,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
            result: ExecutionTaskResult::Cancelled {
                reason: "superseded_by_plan_revision".to_string(),
            },
        }),
    };

    let decision = schedule(request(run_uid, plan, BTreeMap::new(), vec![superseded]))
        .expect("cancelled superseded history must not invalidate the active plan");
    let ScheduleDecision::Ready(tasks) = decision else {
        panic!("expected replacement task, got {decision:?}");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].node_id, "replacement");
}

fn request(
    run_uid: Uuid,
    plan: CanonicalExecutionPlan,
    statuses: BTreeMap<String, ExecutionNodeStatus>,
    tasks: Vec<ExecutionTaskProjection>,
) -> ScheduleRequest {
    ScheduleRequest {
        run_uid,
        goal: goal(),
        plan,
        catalog: catalog(),
        run_input: json!({}),
        projection: ExecutionProjection {
            plan_revision: 0,
            node_statuses: statuses,
            tasks,
        },
        config: ExecutionConfig::default(),
        budget_ledger: BudgetLedger::new(ExecutionBudgetLimit {
            max_cost_microusd: Some(u64::MAX),
            max_tokens: Some(u64::MAX),
            max_tasks: Some(10_000),
            max_tool_calls: Some(u64::MAX),
            max_retrieved_bytes: Some(u64::MAX),
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

fn goal() -> ExecutionGoalContract {
    ExecutionGoalContract {
        objective: "Complete the run".to_string(),
        requirements: vec![ExecutionRequirement {
            id: "req_one".to_string(),
            description: "Complete work".to_string(),
        }],
        deliverables: vec![],
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
                id: "semantic".to_string(),
                description: "Verify semantics".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::AgentVerifier {
                    instructions: "Verify the terminal result".to_string(),
                    max_turns: 2,
                },
            },
        ],
    }
}

fn canonical(nodes: Vec<ExecutionNode>) -> CanonicalExecutionPlan {
    let catalog = catalog();
    CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes,
        },
        plan_hash: ExecutionHash::from_bytes([1; 32]),
        catalog_hash: catalog.catalog_hash,
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
        retry: retry(),
        budget: None,
    }
}

fn output_node(dependency: &str) -> ExecutionNode {
    node(
        "output",
        &[dependency],
        ExecutionOperation::Output {
            value: json!({ "$ref": format!("$.nodes.{dependency}.output") }),
        },
    )
}

fn completed_task(run_uid: Uuid, node_id: &str, output: Value) -> ExecutionTaskProjection {
    completed_item_task(run_uid, node_id, "", json!({}), output)
}

fn completed_item_task(
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    input: Value,
    output: Value,
) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(run_uid, node_id, item_key).expect("task id"),
        node_id: node_id.to_string(),
        item_key: item_key.to_string(),
        status: ExecutionTaskStatus::Completed,
        attempt: 1,
        generation: 1,
        input,
        outcome: Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 0,
                tokens: 0,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
            result: ExecutionTaskResult::Completed {
                output,
                citations: vec![],
            },
        }),
    }
}

fn retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

fn capability() -> CapabilityReference {
    CapabilityReference {
        name: "orders.lookup".to_string(),
        version: "v1".to_string(),
    }
}

fn catalog() -> ExecutionCapabilityCatalog {
    let source = CapabilitySource::BuiltInTool {
        name: "orders.lookup".to_string(),
    };
    let capability = ExecutionCapability {
        reference: capability(),
        contract_revision: "contract-v1".to_string(),
        description: "Look up an order".to_string(),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({}),
        action_class: ActionClass::Read,
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        execution_class: ExecutionClass::Data,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: ExecutionEstimate {
            cost_microusd: 7,
            tokens: 11,
            tool_calls: 3,
            retrieved_bytes: 13,
            tasks: 1,
        },
    };
    let catalog_hash =
        catalog_hash(1, std::slice::from_ref(&capability)).expect("catalog fixture should hash");
    ExecutionCapabilityCatalog {
        schema_version: 1,
        capabilities: vec![capability],
        catalog_hash,
    }
}
