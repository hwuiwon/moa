use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use chrono::{TimeZone, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit,
    ExecutionCancelPolicy, ExecutionCondition, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionPlanDefinition, ExecutionReducer, ExecutionReference,
    ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget,
    ExecutionUsage, MapTask, RetryPolicy,
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
    interpreter::{
        ReduceMaterializationPageInput, ScheduleRequest, materialize_node_page,
        resolve_temporal_target,
    },
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionRunStatus, ExecutionTaskId,
        ExecutionTaskProjection, ExecutionTaskStatus, input_resume_counters,
        retry_dispatch_counters, run_status_after_task_outcome, supersede_waiting_replan,
        task_status_from_outcome, validate_outcome_generation,
    },
};
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn controller_materializes_every_map_item_with_stable_typed_keys() {
    // Pins: max_items is accounting, not a hidden active-worker cap, and one page keeps the
    // plan's item order so the persisted cursor addresses the same item on every replay.
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
            compensation: None,
            retry: retry(),
            budget: None,
        },
        output_node("inspect"),
    ]);

    let page = materialize_node_page(
        &request(run_uid, plan, BTreeMap::new(), vec![]),
        "inspect",
        &BTreeMap::new(),
        0,
        3,
        None,
    )
    .expect("materialize map page");
    assert!(page.source_exhausted);
    assert_eq!(page.next_cursor, 3);
    assert_eq!(
        page.tasks
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>(),
        ["number:1", "string:\"1\"", "object:{\"id\":1}"]
    );
    for task in page.tasks {
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
fn controller_materializes_large_map_in_non_overlapping_cursor_pages() {
    // Pins: controller-facing materialization selects one named node and returns at most
    // 1,000 deterministic tasks without constructing the complete large-map task vector.
    let run_uid = Uuid::from_u128(12);
    let items = (0_u64..2_500).map(|item| json!(item)).collect::<Vec<_>>();
    let plan = canonical(vec![node(
        "inspect",
        &[],
        ExecutionOperation::Map {
            items: Value::Array(items),
            item_key: "".to_string(),
            max_items: 2_500,
            item_output_schema: json!({ "type": "object" }),
            task: MapTask::Capability {
                reference: capability(),
            },
        },
    )]);
    let request = request(run_uid, plan, BTreeMap::new(), Vec::new());
    let first = materialize_node_page(&request, "inspect", &BTreeMap::new(), 0, 1_000, None)
        .expect("first map page");
    let second = materialize_node_page(
        &request,
        "inspect",
        &BTreeMap::new(),
        first.next_cursor,
        1_000,
        None,
    )
    .expect("second map page");
    let third = materialize_node_page(
        &request,
        "inspect",
        &BTreeMap::new(),
        second.next_cursor,
        1_000,
        None,
    )
    .expect("third map page");
    assert_eq!(
        (first.tasks.len(), second.tasks.len(), third.tasks.len()),
        (1_000, 1_000, 500)
    );
    assert_eq!(
        (first.next_cursor, second.next_cursor, third.next_cursor),
        (1_000, 2_000, 2_500)
    );
    assert!(!first.source_exhausted);
    assert!(!second.source_exhausted);
    assert!(third.source_exhausted);
    let ids = first
        .tasks
        .iter()
        .chain(&second.tasks)
        .chain(&third.tasks)
        .map(|task| task.task_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), 2_500);
}

#[test]
fn controller_pages_more_than_twenty_five_hundred_reduce_batches_across_rounds() {
    // Pins: a persisted round/batch cursor materializes every reducer batch exactly once across
    // 1,000-task activation bounds, then starts the next round from its own zero-based cursor.
    let run_uid = Uuid::from_u128(120);
    let items = (0_u64..5_001).map(|item| json!(item)).collect::<Vec<_>>();
    let mut reduce = node(
        "reduce",
        &[],
        ExecutionOperation::Reduce {
            items: Value::Array(items),
            max_items: 5_001,
            reducer: moa_artifacts::execution_plan::ExecutionReducer::Capability {
                reference: capability(),
            },
            batch_size: 2,
        },
    );
    reduce.output_schema = json!({});
    let request = request(
        run_uid,
        canonical(vec![reduce]),
        BTreeMap::new(),
        Vec::new(),
    );

    let first = materialize_node_page(
        &request,
        "reduce",
        &BTreeMap::new(),
        0,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 1,
            batch_cursor: 0,
            round_input_count: None,
            page_inputs: Vec::new(),
        }),
    )
    .expect("first reduce page");
    let second = materialize_node_page(
        &request,
        "reduce",
        &BTreeMap::new(),
        1_000,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 1,
            batch_cursor: 1_000,
            round_input_count: Some(5_001),
            page_inputs: Vec::new(),
        }),
    )
    .expect("second reduce page");
    let third = materialize_node_page(
        &request,
        "reduce",
        &BTreeMap::new(),
        2_000,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 1,
            batch_cursor: 2_000,
            round_input_count: Some(5_001),
            page_inputs: Vec::new(),
        }),
    )
    .expect("final reduce page");
    assert_eq!(
        (first.tasks.len(), second.tasks.len(), third.tasks.len()),
        (1_000, 1_000, 501)
    );
    assert_eq!(
        (first.next_cursor, second.next_cursor, third.next_cursor),
        (1_000, 2_000, 2_501)
    );
    assert_eq!(first.tasks[0].item_key, "r1:b0");
    assert_eq!(third.tasks[500].item_key, "r1:b2500");
    assert!(!first.source_exhausted);
    assert!(!second.source_exhausted);
    assert!(third.source_exhausted);
    assert_eq!(
        first.reduce_cursor.expect("concrete first-round fence"),
        moa_execution::ReduceMaterializationCursor {
            round: 1,
            batch_cursor: 0,
            round_input_count: 5_001,
        }
    );

    let prior_round_outputs = (0_u64..2_501).map(|value| json!(value)).collect::<Vec<_>>();
    let round_two_first = materialize_node_page(
        &request,
        "reduce",
        &BTreeMap::new(),
        2_501,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 2,
            batch_cursor: 0,
            round_input_count: Some(2_501),
            page_inputs: prior_round_outputs[..2_000].to_vec(),
        }),
    )
    .expect("first second-round page");
    let round_two_second = materialize_node_page(
        &request,
        "reduce",
        &BTreeMap::new(),
        3_501,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 2,
            batch_cursor: 1_000,
            round_input_count: Some(2_501),
            page_inputs: prior_round_outputs[2_000..].to_vec(),
        }),
    )
    .expect("final second-round page");
    assert_eq!(
        (round_two_first.tasks.len(), round_two_second.tasks.len()),
        (1_000, 251)
    );
    assert_eq!(round_two_first.tasks[0].item_key, "r2:b0");
    assert_eq!(round_two_second.tasks[250].item_key, "r2:b1250");
    assert!(!round_two_first.source_exhausted);
    assert!(round_two_second.source_exhausted);
}

#[test]
fn controller_rejects_duplicate_dynamic_map_keys() {
    // Pins: duplicate item identities fail materialization before any task can be returned,
    // because two items sharing an item key would collide on one derived task ID.
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
            compensation: None,
            retry: retry(),
            budget: None,
        },
        output_node("inspect"),
    ]);
    let error = materialize_node_page(
        &request(Uuid::from_u128(12), plan, BTreeMap::new(), vec![]),
        "inspect",
        &BTreeMap::new(),
        0,
        2,
        None,
    )
    .expect_err("duplicate item keys must fail the page");
    assert!(
        error.to_string().contains("duplicate item key"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_false_condition_short_circuits_before_map_or_reduce_paging() {
    // Pins: `when` is evaluated in the wrapper, so a false branch never resolves map items
    // and never opens a reduce round. Both sources below are deliberately unmaterializable —
    // the map exceeds its own `max_items` and the reduce is handed no round cursor — so a
    // condition consulted anywhere later than the wrapper surfaces as a hard error here.
    let condition = |value: &str| {
        Some(ExecutionCondition::Equals {
            reference: ExecutionReference {
                path: "$.input.route".to_string(),
            },
            value: json!(value),
        })
    };
    let build = |when, is_reduce: bool| {
        let mut node = ExecutionNode {
            id: "branch".to_string(),
            requirement_ids: vec!["req_one".to_string()],
            depends_on: vec![],
            when,
            input: json!({ "$item": true }),
            output_schema: json!({ "type": "object" }),
            operation: ExecutionOperation::Map {
                items: json!([{ "id": "a" }, { "id": "b" }, { "id": "c" }]),
                item_key: "/id".to_string(),
                max_items: 1,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability(),
                },
            },
            compensation: None,
            retry: retry(),
            budget: None,
        };
        if is_reduce {
            node.input = json!({});
            node.operation = ExecutionOperation::Reduce {
                items: json!([1, 2, 3, 4]),
                max_items: 4,
                reducer: ExecutionReducer::Capability {
                    reference: capability(),
                },
                batch_size: 2,
            };
        }
        node
    };

    for is_reduce in [false, true] {
        let mut skipped = request(
            Uuid::from_u128(41),
            canonical(vec![
                build(condition("taken"), is_reduce),
                output_node("branch"),
            ]),
            BTreeMap::new(),
            vec![],
        );
        skipped.run_input = json!({ "route": "not-taken" });
        let page = materialize_node_page(&skipped, "branch", &BTreeMap::new(), 0, 8, None)
            .expect("a false condition must page without touching the node source");
        assert!(page.condition_skipped);
        assert!(page.tasks.is_empty());
        assert!(page.source_exhausted);
        assert_eq!(page.next_cursor, 0);
        assert!(page.terminal_output.is_none());
        assert!(page.reduce_cursor.is_none());

        let mut taken = request(
            Uuid::from_u128(42),
            canonical(vec![
                build(condition("taken"), is_reduce),
                output_node("branch"),
            ]),
            BTreeMap::new(),
            vec![],
        );
        taken.run_input = json!({ "route": "taken" });
        assert!(
            materialize_node_page(&taken, "branch", &BTreeMap::new(), 0, 8, None).is_err(),
            "the same source must still be reached when the condition holds"
        );
    }
}

#[test]
fn controller_builds_exact_hierarchical_reducer_batch_inputs() {
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
    let page = materialize_node_page(
        &request(Uuid::from_u128(13), plan, BTreeMap::new(), vec![]),
        "reduce",
        &BTreeMap::new(),
        0,
        1_000,
        Some(&ReduceMaterializationPageInput {
            round: 1,
            batch_cursor: 0,
            round_input_count: None,
            page_inputs: Vec::new(),
        }),
    )
    .expect("materialize first reduce round");
    assert_eq!(
        page.tasks
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>(),
        ["r1:b0", "r1:b1", "r1:b2"]
    );
    assert_eq!(
        page.tasks[0].input,
        json!({ "round": 1, "batch_index": 0, "items": [1, 2] })
    );
    assert_eq!(
        page.tasks[2].input,
        json!({ "round": 1, "batch_index": 2, "items": [5] })
    );
}

#[test]
fn controller_validates_capability_input_and_reservation_against_the_pinned_catalog() {
    // Pins: materialization resolves a capability task's reservation and input schema from the
    // catalog snapshot pinned to the run, and refuses a catalog whose estimates would let one
    // logical task consume more than one task budget unit.
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
    let page = materialize_node_page(
        &request(run_uid, plan.clone(), BTreeMap::new(), vec![]),
        "lookup",
        &BTreeMap::new(),
        0,
        1,
        None,
    )
    .expect("matching catalog should materialize");
    assert_eq!(
        page.tasks[0].reservation,
        ExecutionEstimate {
            cost_microusd: 14,
            tokens: 22,
            tool_calls: 6,
            retrieved_bytes: 26,
            tasks: 1,
        }
    );

    let mut invalid_input = request(run_uid, plan.clone(), BTreeMap::new(), vec![]);
    invalid_input.catalog.capabilities[0].input_schema = json!({
        "type": "object",
        "required": ["customer_id"]
    });
    invalid_input.catalog.catalog_hash =
        catalog_hash(&invalid_input.catalog.capabilities).expect("catalog should hash");
    invalid_input.plan.catalog_hash = invalid_input.catalog.catalog_hash;
    let error = materialize_node_page(&invalid_input, "lookup", &BTreeMap::new(), 0, 1, None)
        .expect_err("resolved capability input must validate");
    assert!(matches!(error, moa_execution::Error::Schema { .. }));

    let mut invalid_task_count = request(run_uid, plan, BTreeMap::new(), vec![]);
    invalid_task_count.catalog.capabilities[0].estimate.tasks = 2;
    invalid_task_count.catalog.catalog_hash =
        catalog_hash(&invalid_task_count.catalog.capabilities).expect("catalog should hash");
    invalid_task_count.plan.catalog_hash = invalid_task_count.catalog.catalog_hash;
    let error = materialize_node_page(&invalid_task_count, "lookup", &BTreeMap::new(), 0, 1, None)
        .expect_err("catalog capability estimates must reserve one task");
    assert!(error.to_string().contains("exactly one logical task"));
}

#[test]
fn relative_temporal_targets_resolve_at_wait_entry_and_fence_on_the_run_deadline() {
    // Pins: a wait-entry-relative target is resolved once against the exact entry instant and
    // fails closed rather than persisting a due time the run deadline would never reach.
    let entered_at = Utc
        .with_ymd_and_hms(2026, 7, 13, 2, 0, 0)
        .single()
        .expect("wait entry");
    let deadline_at = Utc
        .with_ymd_and_hms(2026, 7, 13, 4, 0, 0)
        .single()
        .expect("run deadline");
    assert_eq!(
        resolve_temporal_target(
            &ExecutionTemporalTarget::After {
                delay_seconds: 3_600
            },
            entered_at,
            deadline_at,
        )
        .expect("relative target should fit"),
        Utc.with_ymd_and_hms(2026, 7, 13, 3, 0, 0)
            .single()
            .expect("resolved due time")
    );
    assert!(
        resolve_temporal_target(
            &ExecutionTemporalTarget::After {
                delay_seconds: 7_200
            },
            entered_at,
            deadline_at,
        )
        .is_err(),
        "a relative target at the deadline must fail closed"
    );
}

#[test]
fn long_horizon_statuses_use_canonical_snake_case_labels() {
    // Pins: persistence and wire projections use one closed label for each new lifecycle state.
    for (status, label) in [
        (ExecutionRunStatus::WaitingSignal, "waiting_signal"),
        (ExecutionRunStatus::WaitingTimer, "waiting_timer"),
        (ExecutionRunStatus::WaitingExternal, "waiting_external"),
        (ExecutionRunStatus::PauseRequested, "pause_requested"),
        (ExecutionRunStatus::Pausing, "pausing"),
        (ExecutionRunStatus::Paused, "paused"),
    ] {
        assert_eq!(status.as_str(), label);
        assert_eq!(
            ExecutionRunStatus::from_str(label).expect("run label"),
            status
        );
        assert_eq!(
            serde_json::to_value(status).expect("serialize run status"),
            label
        );
    }
    for (status, label) in [
        (ExecutionTaskStatus::Ready, "ready"),
        (ExecutionTaskStatus::Dispatching, "dispatching"),
        (ExecutionTaskStatus::WaitingReview, "waiting_review"),
        (ExecutionTaskStatus::WaitingSignal, "waiting_signal"),
        (ExecutionTaskStatus::WaitingTimer, "waiting_timer"),
        (ExecutionTaskStatus::WaitingExternal, "waiting_external"),
        (ExecutionTaskStatus::UnknownOutcome, "unknown_outcome"),
    ] {
        assert_eq!(status.as_str(), label);
        assert_eq!(
            ExecutionTaskStatus::from_str(label).expect("task label"),
            status
        );
        assert_eq!(
            serde_json::to_value(status).expect("serialize task status"),
            label
        );
    }
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
    assert_eq!(
        task_status_from_outcome(
            &outcome(ExecutionTaskResult::UnknownOutcome {
                message: "provider outcome is ambiguous".to_string(),
            }),
            false,
        ),
        ExecutionTaskStatus::UnknownOutcome
    );
    let completed = outcome(ExecutionTaskResult::Completed {
        output: json!({}),
        citations: vec![],
    });
    for waiting in [
        ExecutionRunStatus::WaitingInput,
        ExecutionRunStatus::WaitingReview,
        ExecutionRunStatus::WaitingSignal,
        ExecutionRunStatus::WaitingTimer,
        ExecutionRunStatus::WaitingExternal,
    ] {
        assert_eq!(
            run_status_after_task_outcome(waiting, &completed),
            ExecutionRunStatus::Running
        );
    }

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
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
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
        compensation: None,
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
        async_mode: moa_core::types::tools::ToolAsyncMode::SynchronousOnly,
        execution_class: ExecutionClass::Data,
        requires_sandbox: false,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: ExecutionEstimate {
            cost_microusd: 7,
            tokens: 11,
            tool_calls: 3,
            retrieved_bytes: 13,
            tasks: 1,
        },
        rollback: None,
    };
    let catalog_hash =
        catalog_hash(std::slice::from_ref(&capability)).expect("catalog fixture should hash");
    ExecutionCapabilityCatalog {
        capabilities: vec![capability],
        catalog_hash,
    }
}
