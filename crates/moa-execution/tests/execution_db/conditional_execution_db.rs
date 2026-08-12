//! Durable conditional-branch contracts for `ExecutionNode.when`.

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionCondition, ExecutionFailureClass, ExecutionNode,
    ExecutionOperation, ExecutionReducer, ExecutionReference, MapTask,
};
use moa_config::ExecutionConfig;
use moa_execution::budget::BudgetLedger;
use moa_execution::interpreter::{
    NodeMaterializationPage, ReduceMaterializationPageInput, ScheduleRequest, materialize_node_page,
};
use moa_execution::repository::ready::{
    ExecutionReduceMaterializationCursor, ReadyMaterializationOutcome, ReadyMaterializationRequest,
};
use moa_execution::repository::task::{
    TaskAttemptFence, TaskAttemptReleaseClaimOutcome, TaskAttemptSettlementOutcome,
    TaskAttemptStartOutcome,
};
use moa_execution::state::{ExecutionProjection, failed_task_outcome};
use serde_json::Value;
use std::collections::BTreeMap;

use super::support::*;

fn branch_node(id: &str, when: Option<ExecutionCondition>) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: Vec::new(),
        when,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output {
            value: json!({ "branch": id }),
        },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    }
}

fn input_equals(field: &str, value: Value) -> Option<ExecutionCondition> {
    Some(ExecutionCondition::Equals {
        reference: ExecutionReference {
            path: format!("$.input.{field}"),
        },
        value,
    })
}

/// Drives one bounded controller pass over every dependency-ready node.
///
/// This is the exact production pairing the controller performs — pure
/// `materialize_node_page` feeding the durable `materialize_ready_page` transaction —
/// so a condition that is never consulted, or a skip that is never committed, shows up
/// here as materialized tasks rather than as a passing assertion.
async fn advance_ready_nodes(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    only: Option<&str>,
) -> Result<Vec<(String, ReadyMaterializationOutcome)>, Box<dyn std::error::Error + Send + Sync>> {
    let config = ExecutionConfig::default();
    let projection = repository
        .load_activation_projection(scope, run_uid, 64)
        .await?
        .expect("run must remain visible");
    let run = &projection.run;
    let mut outcomes = Vec::new();
    for node in &projection.nodes {
        if only.is_some_and(|wanted| wanted != node.node_id) {
            continue;
        }
        let plan_node = run
            .active_plan
            .definition
            .nodes
            .iter()
            .find(|plan_node| plan_node.id == node.node_id)
            .expect("activation node must exist in the active plan");
        let schedule = ScheduleRequest {
            run_uid,
            goal: run.goal.clone(),
            plan: run.active_plan.clone(),
            catalog: run.catalog.clone(),
            run_input: run.input.clone(),
            projection: ExecutionProjection {
                plan_revision: run.plan_revision,
                node_statuses: BTreeMap::new(),
                tasks: Vec::new(),
            },
            config: config.clone(),
            budget_ledger: BudgetLedger {
                limit: run.approved_budget.clone(),
                reserved: run.reserved,
                consumed: run.consumed,
                overrun: run.budget_overrun,
            },
            now: Utc::now(),
        };
        let reduce_input =
            matches!(plan_node.operation, ExecutionOperation::Reduce { .. }).then(|| {
                ReduceMaterializationPageInput {
                    round: node.reduce_round,
                    batch_cursor: node.reduce_batch_cursor,
                    round_input_count: node.reduce_round_input_count,
                    page_inputs: Vec::new(),
                }
            });
        let NodeMaterializationPage {
            tasks,
            source_exhausted,
            reduce_cursor,
            terminal_output,
            condition_skipped,
            ..
        } = materialize_node_page(
            &schedule,
            &node.node_id,
            &projection.referenced_outputs,
            node.materialization_cursor,
            64,
            reduce_input.as_ref(),
        )?;
        let outcome = repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid,
                    plan_revision: run.plan_revision,
                    node_id: node.node_id.clone(),
                    expected_cursor: node.materialization_cursor,
                    reduce_cursor: reduce_cursor.map(|cursor| {
                        ExecutionReduceMaterializationCursor {
                            round: cursor.round,
                            batch_cursor: cursor.batch_cursor,
                            round_input_count: cursor.round_input_count,
                        }
                    }),
                    source_exhausted,
                    terminal_output,
                    condition_skipped,
                    tasks,
                },
            )
            .await?;
        outcomes.push((node.node_id.clone(), outcome));
    }
    Ok(outcomes)
}

/// Fails the run's single admitted attempt terminally through the real settlement path.
async fn fail_worker(
    repository: &ExecutionRepository,
    config: &ExecutionConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let admission = repository
        .admit_ready_attempts(config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("the unconditional branch must be admissible");
    let fence = TaskAttemptFence {
        tenant_id: admission.tenant_id,
        run_uid: admission.run_uid,
        task_id: admission.task_id,
        controller_generation: admission.controller_generation,
        attempt_generation: admission.attempt_generation,
        dispatch_uid: admission.dispatch_uid,
        capacity_reservation_uid: admission.capacity_reservation_uid,
        watchdog_trigger_uid: admission.watchdog_trigger_uid,
        attempt_deadline_at: admission.attempt_deadline_at,
    };
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("the admitted attempt must start");
    };
    let settled_at = Utc::now();
    assert!(matches!(
        repository
            .begin_task_attempt_release(fence, started.task.generation, "terminal", settled_at)
            .await?,
        TaskAttemptReleaseClaimOutcome::Applied(_)
    ));
    let outcome = failed_task_outcome(
        ExecutionFailureClass::Terminal,
        "worker failed terminally".to_string(),
        usage(1),
    );
    assert!(matches!(
        repository
            .settle_released_task_attempt(config, fence, outcome, None, settled_at, None)
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    Ok(())
}

/// Exact durable scheduler aggregate for one node, read straight from its row.
struct NodeState {
    status: String,
    remaining_dependency_count: i64,
    total_task_count: i64,
    materialization_complete: bool,
    aggregate_complete: bool,
    aggregate_output: Option<Value>,
    aggregate_output_hash: Option<String>,
    reduce_round: i64,
}

async fn node_state(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    node_id: &str,
) -> Result<NodeState, Box<dyn std::error::Error + Send + Sync>> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            i64,
            i64,
            bool,
            bool,
            Option<Value>,
            Option<String>,
            i64,
        ),
    >(
        "SELECT node_status, remaining_dependency_count, total_task_count, \
                materialization_complete, aggregate_complete, aggregate_output, \
                aggregate_output_hash, reduce_round \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id=$2",
    )
    .bind(run_uid)
    .bind(node_id)
    .fetch_one(pool)
    .await?;
    Ok(NodeState {
        status: row.0,
        remaining_dependency_count: row.1,
        total_task_count: row.2,
        materialization_complete: row.3,
        aggregate_complete: row.4,
        aggregate_output: row.5,
        aggregate_output_hash: row.6,
        reduce_round: row.7,
    })
}

#[tokio::test]
async fn a_false_node_condition_skips_only_its_own_branch_db() -> TestResult {
    // Pins: exactly one branch of a two-branch plan materializes work; the branch whose
    // condition is false commits `skipped` with a null aggregate and never creates a task.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "conditional-branches",
        ExecutionRunStatus::Queued,
        budget(64),
    );
    candidate.input = json!({ "route": "escalate" });
    let mut join = branch_node("join", None);
    join.depends_on = vec!["escalate".to_string(), "standard".to_string()];
    candidate.plan.definition.nodes = vec![
        branch_node("escalate", input_equals("route", json!("escalate"))),
        branch_node("standard", input_equals("route", json!("standard"))),
        join,
    ];
    let run = create_run(&repository, scope, candidate).await?;

    let outcomes = advance_ready_nodes(&repository, scope, run.run_uid, None).await?;
    assert_eq!(
        outcomes
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        vec!["escalate", "standard"],
        "only dependency-ready nodes may be advanced"
    );
    let escalate_tasks = match &outcomes[0].1 {
        ReadyMaterializationOutcome::Applied { tasks, .. } => tasks.len(),
        outcome => panic!("the true branch must apply one page: {outcome:?}"),
    };
    assert_eq!(
        escalate_tasks, 1,
        "the true branch must materialize its task"
    );
    let standard_tasks = match &outcomes[1].1 {
        ReadyMaterializationOutcome::Applied { tasks, .. } => tasks.len(),
        outcome => panic!("the false branch must apply its skip: {outcome:?}"),
    };
    assert_eq!(
        standard_tasks, 0,
        "a false condition must not materialize any logical task"
    );

    let skipped = node_state(&pool, run.run_uid, "standard").await?;
    assert_eq!(skipped.status, "skipped");
    assert_eq!(skipped.total_task_count, 0);
    assert!(skipped.materialization_complete);
    assert!(skipped.aggregate_complete);
    assert_eq!(skipped.aggregate_output, Some(Value::Null));
    assert_eq!(
        skipped.aggregate_output_hash,
        Some(moa_execution::capability::node_output_hash(&Value::Null)?.to_string()),
        "a skipped aggregate must carry the verified hash dependents check on load"
    );

    let taken = node_state(&pool, run.run_uid, "escalate").await?;
    assert_eq!(taken.status, "ready");
    assert_eq!(taken.total_task_count, 1);

    let listed = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    assert_eq!(
        listed
            .tasks
            .iter()
            .map(|task| task.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["escalate"],
        "the skipped branch must own no task row at all"
    );
    Ok(())
}

#[tokio::test]
async fn a_fan_in_of_two_skipped_branches_releases_its_dependent_exactly_once_db() -> TestResult {
    // Pins: each skip decrements the fan-in counter once, and replaying either skip page
    // returns Replayed without decrementing again.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "conditional-fan-in",
        ExecutionRunStatus::Queued,
        budget(64),
    );
    candidate.input = json!({ "route": "neither" });
    let mut join = branch_node("join", None);
    join.depends_on = vec!["escalate".to_string(), "standard".to_string()];
    candidate.plan.definition.nodes = vec![
        branch_node("escalate", input_equals("route", json!("escalate"))),
        branch_node("standard", input_equals("route", json!("standard"))),
        join,
    ];
    let run = create_run(&repository, scope, candidate).await?;

    assert_eq!(
        node_state(&pool, run.run_uid, "join")
            .await?
            .remaining_dependency_count,
        2
    );
    advance_ready_nodes(&repository, scope, run.run_uid, None).await?;
    let released = node_state(&pool, run.run_uid, "join").await?;
    assert_eq!(
        released.remaining_dependency_count, 0,
        "both skipped branches must release the fan-in exactly once each"
    );
    assert_eq!(released.status, "pending");

    for node_id in ["escalate", "standard"] {
        let replayed = repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    node_id: node_id.to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: true,
                    tasks: Vec::new(),
                },
            )
            .await?;
        assert!(
            matches!(replayed, ReadyMaterializationOutcome::Replayed { .. }),
            "a committed skip must replay rather than reapply: {replayed:?}"
        );
    }
    let after_replay = node_state(&pool, run.run_uid, "join").await?;
    assert_eq!(
        after_replay.remaining_dependency_count, 0,
        "replaying a skip must not decrement the fan-in a second time"
    );
    let skipped = node_state(&pool, run.run_uid, "escalate").await?;
    assert_eq!(skipped.status, "skipped");
    assert_eq!(skipped.total_task_count, 0);
    Ok(())
}

#[tokio::test]
async fn a_dependent_of_a_skipped_and_a_failed_branch_cancels_without_raising_db() -> TestResult {
    // Pins: the diamond where one dependency is skipped and a sibling fails terminally, in
    // both settle orders. The cancellation cascade asserts every descendant is pending with
    // zero tasks, and a released-then-cancelled dependent must not lose its counter.
    for skip_first in [true, false] {
        let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
        let pool = test_db.store().pool().clone();
        let repository = ExecutionRepository::new(pool.clone());
        let tenant_id = TenantId::new();
        let scope = ExecutionScope::Tenant { tenant_id };
        let mut candidate = new_run(
            tenant_id,
            None,
            "conditional-diamond",
            ExecutionRunStatus::Queued,
            budget(64),
        );
        candidate.input = json!({ "route": "neither" });
        let mut join = branch_node("join", None);
        join.depends_on = vec!["worker".to_string(), "conditional".to_string()];
        candidate.plan.definition.nodes = vec![
            branch_node("worker", None),
            branch_node("conditional", input_equals("route", json!("taken"))),
            join,
        ];
        let run = create_run(&repository, scope, candidate).await?;
        if skip_first {
            advance_ready_nodes(&repository, scope, run.run_uid, Some("conditional")).await?;
            advance_ready_nodes(&repository, scope, run.run_uid, Some("worker")).await?;
            fail_worker(&repository, &ExecutionConfig::default()).await?;
        } else {
            advance_ready_nodes(&repository, scope, run.run_uid, Some("worker")).await?;
            fail_worker(&repository, &ExecutionConfig::default()).await?;
            advance_ready_nodes(&repository, scope, run.run_uid, Some("conditional")).await?;
        }

        let join_state = node_state(&pool, run.run_uid, "join").await?;
        assert_eq!(
            join_state.status, "cancelled",
            "a dependent of a terminally failed branch must be cancelled"
        );
        assert_eq!(join_state.remaining_dependency_count, 0);
        assert_eq!(join_state.total_task_count, 0);
        assert_eq!(
            node_state(&pool, run.run_uid, "conditional").await?.status,
            "skipped"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_false_condition_skips_map_and_reduce_nodes_before_any_paging_db() -> TestResult {
    // Pins: an aggregate node whose condition is false never opens a map page or a reduce
    // round, and its completed aggregate keeps it out of the pending map-aggregate queue.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "conditional-aggregates",
        ExecutionRunStatus::Queued,
        budget(64),
    );
    candidate.input = json!({ "route": "neither" });
    let mut map = branch_node("map", input_equals("route", json!("taken")));
    map.operation = ExecutionOperation::Map {
        items: json!([1, 2, 3]),
        item_key: String::new(),
        max_items: 3,
        item_output_schema: json!({}),
        task: MapTask::Capability {
            reference: CapabilityReference {
                name: "test.map".to_string(),
                version: "v1".to_string(),
            },
        },
    };
    let mut reduce = branch_node("reduce", input_equals("route", json!("taken")));
    reduce.operation = ExecutionOperation::Reduce {
        items: json!([1, 2, 3, 4]),
        max_items: 4,
        reducer: ExecutionReducer::Capability {
            reference: CapabilityReference {
                name: "test.reduce".to_string(),
                version: "v1".to_string(),
            },
        },
        batch_size: 2,
    };
    candidate.plan.definition.nodes = vec![map, reduce];
    let run = create_run(&repository, scope, candidate).await?;

    advance_ready_nodes(&repository, scope, run.run_uid, None).await?;

    for node_id in ["map", "reduce"] {
        let state = node_state(&pool, run.run_uid, node_id).await?;
        assert_eq!(
            state.status, "skipped",
            "conditional {node_id} node must skip"
        );
        assert_eq!(state.total_task_count, 0);
        assert!(state.materialization_complete);
        assert!(
            state.aggregate_complete,
            "a skipped aggregate must be final so nothing re-aggregates it"
        );
        assert_eq!(state.aggregate_output, Some(Value::Null));
    }
    assert_eq!(
        node_state(&pool, run.run_uid, "reduce").await?.reduce_round,
        1,
        "a skipped reduce must never open a later round"
    );

    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("conditional aggregate run");
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                run.run_uid,
                current.controller_generation,
                current.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    assert!(
        repository
            .load_map_aggregate_candidate(
                scope,
                run.run_uid,
                current.controller_generation,
                current.wake_epoch,
            )
            .await?
            .is_none(),
        "a skipped map must not enter the pending map-aggregate queue"
    );
    Ok(())
}
