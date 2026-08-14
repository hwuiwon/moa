//! Bounded incremental scheduler projection and materialization contracts.

use moa_artifacts::execution_plan::{
    CapabilityReference, ExecutionNode, ExecutionOperation, ExecutionReducer, MapTask,
};
use moa_config::ExecutionConfig;
use moa_execution::repository::capacity::ExecutionAdmissionItem;
use moa_execution::repository::ready::{
    ExecutionNodeQueueStatus, ExecutionReduceMaterializationCursor, MapAggregatePageOutcome,
    MapAggregatePageRequest, ReadyMaterializationOutcome, ReadyMaterializationRequest,
};
use moa_execution::repository::task::{TaskAttemptFence, TaskAttemptStartOutcome};
use moa_execution::repository::{TransitionOutcome, TransitionRejection};
use moa_execution::state::completed_task_outcome;
use serde_json::Value;

use super::support::*;

fn output_node(id: &str) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    }
}

fn reduce_node(id: &str) -> ExecutionNode {
    let mut node = output_node(id);
    node.operation = ExecutionOperation::Reduce {
        items: Value::Array((0_u64..5_001).map(|item| json!(item)).collect()),
        max_items: 5_001,
        reducer: ExecutionReducer::Capability {
            reference: CapabilityReference {
                name: "test.reduce".to_string(),
                version: "v1".to_string(),
            },
        },
        batch_size: 2,
    };
    node
}

fn empty_map_node(id: &str) -> ExecutionNode {
    let mut node = output_node(id);
    node.operation = ExecutionOperation::Map {
        items: json!([]),
        item_key: String::new(),
        max_items: 1,
        item_output_schema: json!({}),
        task: MapTask::Capability {
            reference: CapabilityReference {
                name: "test.map".to_string(),
                version: "v1".to_string(),
            },
        },
    };
    node
}

fn map_node(id: &str, max_items: u32) -> ExecutionNode {
    let mut node = empty_map_node(id);
    let ExecutionOperation::Map {
        max_items: stored_max_items,
        ..
    } = &mut node.operation
    else {
        unreachable!("empty-map fixture must remain a map")
    };
    *stored_max_items = u64::from(max_items);
    node
}

async fn force_map_tasks_terminal(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    node_id: &str,
    output: Option<&Value>,
) -> TestResult {
    for (status, attempt_state) in [("dispatching", "dispatching"), ("running", "running")] {
        sqlx::query(
            "UPDATE moa.execution_task SET status=$3,attempt_state=$4,updated_at=NOW() \
             WHERE run_uid=$1 AND node_id=$2",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(status)
        .bind(attempt_state)
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "UPDATE moa.execution_task SET status='completed',attempt_state='terminal', \
             output=COALESCE($3::JSONB,to_jsonb(item_key)),completed_at=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND node_id=$2",
    )
    .bind(run_uid)
    .bind(node_id)
    .bind(output)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='pending',ready_task_count=0, \
             terminal_task_count=total_task_count,succeeded_task_count=total_task_count, \
             aggregate_output=NULL,aggregate_output_hash=NULL,aggregate_cursor_item_key=NULL, \
             aggregate_complete=FALSE,updated_at=NOW() WHERE run_uid=$1 AND node_id=$2",
    )
    .bind(run_uid)
    .bind(node_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status='running',ready_task_count=0,active_task_count=0, \
             updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run_uid)
    .execute(pool)
    .await?;
    Ok(())
}

#[tokio::test]
async fn ten_thousand_tasks_materialize_in_cursor_fenced_pages_db() -> TestResult {
    // Pins: a large map-sized logical node never requires one unbounded task vector or
    // full-task scheduling snapshot; each committed page is at most 1,000 tasks.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "incremental-10k",
        ExecutionRunStatus::Queued,
        budget(20_000),
    );
    candidate.plan.definition.nodes = vec![output_node("collect")];
    let run = create_run(&repository, scope, candidate).await?;
    let mut cursor = 0_u64;
    for page in 0_u64..10 {
        let tasks = (0_u64..1_000)
            .map(|offset| {
                logical_task(
                    run.run_uid,
                    "collect",
                    &format!("item-{:05}", page * 1_000 + offset),
                    estimate(1),
                )
            })
            .collect::<Vec<_>>();
        let ReadyMaterializationOutcome::Applied {
            tasks,
            next_cursor,
            triggers,
        } = repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "collect".to_string(),
                    expected_cursor: cursor,
                    reduce_cursor: None,
                    source_exhausted: false,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks,
                },
            )
            .await?
        else {
            panic!("a fresh cursor page must apply exactly once");
        };
        assert_eq!(tasks.len(), 1_000);
        assert!(triggers.is_empty());
        assert_eq!(next_cursor, cursor + 1_000);
        cursor = next_cursor;
    }

    let projection = repository
        .load_activation_projection(scope, run.run_uid, 32)
        .await?
        .expect("run must remain visible");
    assert_eq!(projection.nodes.len(), 1);
    let node = &projection.nodes[0];
    assert_eq!(node.status, ExecutionNodeQueueStatus::Ready);
    assert_eq!(node.materialization_cursor, 10_000);
    assert_eq!(node.total_task_count, 10_000);
    assert_eq!(node.ready_task_count, 10_000);
    assert_eq!(projection.run.ready_task_count, 10_000);

    let verification = repository
        .load_terminal_verification_page(scope, run.run_uid, None, 64)
        .await?;
    assert_eq!(verification.nonterminal_tasks.len(), 64);
    assert!(verification.next_cursor.is_some());
    Ok(())
}

#[tokio::test]
async fn ten_thousand_map_outputs_aggregate_in_sixteen_row_crash_replay_pages_db() -> TestResult {
    // Pins: a 10,000-item map builds its deterministic output through <=16-row persisted pages;
    // replaying a committed cursor cannot duplicate values and dependencies release only once.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "map-aggregate-10k",
        ExecutionRunStatus::Queued,
        budget(20_000),
    );
    let mut dependent = output_node("dependent");
    dependent.depends_on = vec!["map".to_string()];
    candidate.plan.definition.nodes = vec![map_node("map", 10_000), dependent];
    let run = create_run(&repository, scope, candidate).await?;

    let mut cursor = 0_u64;
    for page in 0_u64..10 {
        let tasks = (0_u64..1_000)
            .map(|offset| {
                logical_task(
                    run.run_uid,
                    "map",
                    &format!("item-{:05}", page * 1_000 + offset),
                    estimate(1),
                )
            })
            .collect::<Vec<_>>();
        let ReadyMaterializationOutcome::Applied { next_cursor, .. } = repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    node_id: "map".to_string(),
                    expected_cursor: cursor,
                    reduce_cursor: None,
                    source_exhausted: page == 9,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks,
                },
            )
            .await?
        else {
            panic!("fresh map page must apply");
        };
        cursor = next_cursor;
    }
    force_map_tasks_terminal(&pool, run.run_uid, "map", None).await?;
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("map run");
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

    let first = repository
        .load_map_aggregate_candidate(
            scope,
            run.run_uid,
            current.controller_generation,
            current.wake_epoch,
        )
        .await?
        .expect("first aggregate page");
    let first_request = MapAggregatePageRequest {
        run_uid: run.run_uid,
        plan_revision: run.plan_revision,
        controller_generation: current.controller_generation,
        wake_epoch: current.wake_epoch,
        node_id: first.node_id,
        expected_cursor_item_key: first.cursor_item_key,
    };
    let MapAggregatePageOutcome::Applied {
        aggregated_tasks: 16,
        aggregate_complete: false,
        ..
    } = repository
        .advance_map_aggregate_page(scope, first_request.clone())
        .await?
    else {
        panic!("first aggregate page must append sixteen outputs");
    };
    assert!(matches!(
        repository
            .advance_map_aggregate_page(scope, first_request)
            .await?,
        MapAggregatePageOutcome::Replayed {
            aggregate_complete: false,
            ..
        }
    ));

    let mut committed_pages = 1_u32;
    loop {
        let Some(candidate) = repository
            .load_map_aggregate_candidate(
                scope,
                run.run_uid,
                current.controller_generation,
                current.wake_epoch,
            )
            .await?
        else {
            break;
        };
        match repository
            .advance_map_aggregate_page(
                scope,
                MapAggregatePageRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    controller_generation: current.controller_generation,
                    wake_epoch: current.wake_epoch,
                    node_id: candidate.node_id,
                    expected_cursor_item_key: candidate.cursor_item_key,
                },
            )
            .await?
        {
            MapAggregatePageOutcome::Applied {
                aggregated_tasks,
                aggregate_complete,
                ..
            } => {
                assert!((1..=16).contains(&aggregated_tasks));
                committed_pages += 1;
                if aggregate_complete {
                    break;
                }
            }
            other => panic!("unexpected map aggregate outcome: {other:?}"),
        }
        assert!(committed_pages <= 625, "aggregate cursor failed to advance");
    }
    assert_eq!(committed_pages, 625);
    let aggregate = sqlx::query_as::<_, (String, bool, Value, i64)>(
        "SELECT node_status,aggregate_complete,aggregate_output, \
             (SELECT remaining_dependency_count FROM moa.execution_node_state \
              WHERE run_uid=$1 AND node_id='dependent') \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='map'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(aggregate.0, "completed");
    assert!(aggregate.1);
    assert_eq!(aggregate.2.as_array().map(Vec::len), Some(10_000));
    assert_eq!(aggregate.3, 0);
    Ok(())
}

#[tokio::test]
async fn seventeenth_near_sixty_four_kib_map_output_fails_before_unbounded_aggregate_db()
-> TestResult {
    // Pins: sixteen near-limit inline outputs fit one bounded page, while the seventeenth crosses
    // the cumulative one-MiB ceiling, fails the node, and never releases its dependency.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "map-aggregate-overflow",
        ExecutionRunStatus::Queued,
        budget(100),
    );
    let mut dependent = output_node("dependent");
    dependent.depends_on = vec!["map".to_string()];
    candidate.plan.definition.nodes = vec![map_node("map", 17), dependent];
    let run = create_run(&repository, scope, candidate).await?;
    let tasks = (0_u64..17)
        .map(|index| logical_task(run.run_uid, "map", &format!("item-{index:02}"), estimate(1)))
        .collect::<Vec<_>>();
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    node_id: "map".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks,
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let near_limit = Value::String("x".repeat(64_000));
    force_map_tasks_terminal(&pool, run.run_uid, "map", Some(&near_limit)).await?;
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("overflow run");
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
    let first = repository
        .load_map_aggregate_candidate(
            scope,
            run.run_uid,
            current.controller_generation,
            current.wake_epoch,
        )
        .await?
        .expect("overflow first page");
    assert!(matches!(
        repository
            .advance_map_aggregate_page(
                scope,
                MapAggregatePageRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    controller_generation: current.controller_generation,
                    wake_epoch: current.wake_epoch,
                    node_id: first.node_id,
                    expected_cursor_item_key: first.cursor_item_key,
                },
            )
            .await?,
        MapAggregatePageOutcome::Applied {
            aggregated_tasks: 16,
            aggregate_complete: false,
            ..
        }
    ));
    let second = repository
        .load_map_aggregate_candidate(
            scope,
            run.run_uid,
            current.controller_generation,
            current.wake_epoch,
        )
        .await?
        .expect("overflow second page");
    assert_eq!(
        repository
            .advance_map_aggregate_page(
                scope,
                MapAggregatePageRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    controller_generation: current.controller_generation,
                    wake_epoch: current.wake_epoch,
                    node_id: second.node_id,
                    expected_cursor_item_key: second.cursor_item_key,
                },
            )
            .await?,
        MapAggregatePageOutcome::Overflow
    );
    let state = sqlx::query_as::<_, (String, bool, Option<Value>, i64)>(
        "SELECT node_status,aggregate_complete,aggregate_output, \
             (SELECT remaining_dependency_count FROM moa.execution_node_state \
              WHERE run_uid=$1 AND node_id='dependent') \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='map'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, ("failed".to_string(), true, None, 1));
    Ok(())
}

#[tokio::test]
async fn twenty_five_hundred_reduce_batches_persist_exact_round_cursor_db() -> TestResult {
    // Pins: three bounded commits persist 2,501 distinct first-round reducer batches, replay the
    // last page exactly, and reject an off-by-one round cursor without duplicating task rows.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "reduce-2501-batches",
        ExecutionRunStatus::Queued,
        budget(10_000),
    );
    candidate.plan.definition.nodes = vec![reduce_node("reduce")];
    let run = create_run(&repository, scope, candidate).await?;
    let mut total_cursor = 0_u64;
    let mut replay_request = None;
    for (batch_cursor, page_count) in [(0_u64, 1_000_u64), (1_000, 1_000), (2_000, 501)] {
        let tasks = (batch_cursor..batch_cursor + page_count)
            .map(|batch| logical_task(run.run_uid, "reduce", &format!("r1:b{batch}"), estimate(1)))
            .collect::<Vec<_>>();
        let request = ReadyMaterializationRequest {
            run_uid: run.run_uid,
            plan_revision: 1,
            node_id: "reduce".to_string(),
            expected_cursor: total_cursor,
            reduce_cursor: Some(ExecutionReduceMaterializationCursor {
                round: 1,
                batch_cursor,
                round_input_count: 5_001,
            }),
            source_exhausted: batch_cursor + page_count == 2_501,
            terminal_output: None,
            condition_skipped: false,
            tasks,
        };
        let ReadyMaterializationOutcome::Applied { next_cursor, .. } = repository
            .materialize_ready_page(scope, &ExecutionConfig::default(), request.clone())
            .await?
        else {
            panic!("fresh reduce page must apply");
        };
        total_cursor = next_cursor;
        replay_request = Some(request);
    }
    assert_eq!(total_cursor, 2_501);
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                replay_request.expect("last page"),
            )
            .await?,
        ReadyMaterializationOutcome::Replayed {
            next_cursor: 2_501,
            ..
        }
    ));

    let off_by_one = ReadyMaterializationRequest {
        run_uid: run.run_uid,
        plan_revision: 1,
        node_id: "reduce".to_string(),
        expected_cursor: 2_501,
        reduce_cursor: Some(ExecutionReduceMaterializationCursor {
            round: 1,
            batch_cursor: 2_500,
            round_input_count: 5_001,
        }),
        source_exhausted: true,
        terminal_output: None,
        condition_skipped: false,
        tasks: vec![logical_task(run.run_uid, "reduce", "r1:b2500", estimate(1))],
    };
    assert_eq!(
        repository
            .materialize_ready_page(scope, &ExecutionConfig::default(), off_by_one)
            .await?,
        ReadyMaterializationOutcome::Conflict
    );

    let node: (i64, i64, i64, Option<i64>, i64, i64, bool) = sqlx::query_as(
        "SELECT materialization_cursor,reduce_round,reduce_batch_cursor, \
                reduce_round_input_count,reduce_round_task_count, \
                reduce_round_terminal_task_count,reduce_ready \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='reduce'",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(node, (2_501, 1, 2_501, Some(5_001), 2_501, 0, true));
    Ok(())
}

#[tokio::test]
async fn actionable_projection_skips_completed_prefix_larger_than_activation_bound_db() -> TestResult
{
    // Pins: 129 completed source-fenced nodes cannot hide the next actionable node from a
    // controller whose ordinary maximum_activation_steps bound is 128.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "completed-prefix",
        ExecutionRunStatus::Queued,
        budget(1_000),
    );
    candidate.plan.definition.nodes = (0_u32..130)
        .map(|index| output_node(&format!("node-{index:03}")))
        .collect();
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status = 'completed', \
             materialization_complete = TRUE, aggregate_output = 'null'::JSONB, \
             aggregate_output_hash = $2, updated_at = NOW() - INTERVAL '1 hour' \
         WHERE run_uid = $1 AND node_order < 129",
    )
    .bind(run.run_uid)
    .bind(moa_execution::capability::node_output_hash(&Value::Null)?.to_string())
    .execute(&pool)
    .await?;

    let projection = repository
        .load_activation_projection(scope, run.run_uid, 1)
        .await?
        .expect("actionable projection");
    assert_eq!(projection.nodes.len(), 1);
    assert_eq!(projection.nodes[0].node_id, "node-129");
    assert!(!projection.has_more_actionable);
    Ok(())
}

#[tokio::test]
async fn empty_map_completion_cas_releases_its_dependent_without_repeating_db() -> TestResult {
    // Pins: an exhausted empty source commits a terminal aggregate even with zero task rows, and
    // the next controller projection advances to the newly released dependent exactly once.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "empty-map",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    let mut dependent = output_node("after");
    dependent.depends_on = vec!["empty".to_string()];
    candidate.plan.definition.nodes = vec![empty_map_node("empty"), dependent];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;

    let request = ReadyMaterializationRequest {
        run_uid: run.run_uid,
        plan_revision: 1,
        node_id: "empty".to_string(),
        expected_cursor: 0,
        reduce_cursor: None,
        source_exhausted: true,
        terminal_output: Some(json!({ "items": [] })),
        condition_skipped: false,
        tasks: Vec::new(),
    };
    assert!(matches!(
        repository
            .materialize_ready_page(scope, &ExecutionConfig::default(), request.clone())
            .await?,
        ReadyMaterializationOutcome::Applied { tasks, next_cursor: 0, .. }
            if tasks.is_empty()
    ));
    assert!(matches!(
        repository
            .materialize_ready_page(scope, &ExecutionConfig::default(), request)
            .await?,
        ReadyMaterializationOutcome::Replayed { tasks, next_cursor: 0, .. }
            if tasks.is_empty()
    ));

    let projection = repository
        .load_activation_projection(scope, run.run_uid, 8)
        .await?
        .expect("dependent projection");
    assert_eq!(projection.nodes.len(), 1);
    assert_eq!(projection.nodes[0].node_id, "after");
    assert_eq!(projection.nodes[0].remaining_dependency_count, 0);
    let readiness = repository
        .load_activation_readiness(scope, run.run_uid)
        .await?
        .expect("readiness summary");
    assert!(readiness.has_actionable_nodes);
    assert!(readiness.has_unfinished_nodes);
    assert!(!readiness.has_nonterminal_tasks);
    assert!(!readiness.terminal_ready());
    Ok(())
}

#[tokio::test]
async fn terminal_partial_map_page_cannot_complete_node_before_source_exhaustion_db() -> TestResult
{
    // Pins: settling every task in a committed map prefix cannot complete the node or release its
    // dependent until the exact source-exhausted page has also committed and settled.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "partial-map-terminal-fence",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    let mut dependent = output_node("after");
    dependent.depends_on = vec!["map".to_string()];
    candidate.plan.definition.nodes = vec![output_node("map"), dependent];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    let config = ExecutionConfig::default();

    let first = logical_task(run.run_uid, "map", "0000", estimate(1));
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "map".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: false,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![first],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { next_cursor: 1, .. }
    ));
    settle_one_ready_task(&repository, &config, run.run_uid, "0000").await?;

    let partial = repository
        .load_activation_projection(scope, run.run_uid, 8)
        .await?
        .expect("partial map projection");
    assert_eq!(partial.nodes.len(), 1);
    assert_eq!(partial.nodes[0].node_id, "map");
    assert_eq!(partial.nodes[0].status, ExecutionNodeQueueStatus::Pending);
    assert!(!partial.nodes[0].materialization_complete);
    assert_eq!(partial.nodes[0].terminal_task_count, 1);
    assert_eq!(partial.nodes[0].total_task_count, 1);

    let second = logical_task(run.run_uid, "map", "0001", estimate(1));
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "map".to_string(),
                    expected_cursor: 1,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![second],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { next_cursor: 2, .. }
    ));
    settle_one_ready_task(&repository, &config, run.run_uid, "0001").await?;

    let released = repository
        .load_activation_projection(scope, run.run_uid, 8)
        .await?
        .expect("released dependent projection");
    assert_eq!(released.nodes.len(), 1);
    assert_eq!(released.nodes[0].node_id, "after");
    assert_eq!(released.nodes[0].remaining_dependency_count, 0);
    Ok(())
}

async fn settle_one_ready_task(
    repository: &ExecutionRepository,
    config: &ExecutionConfig,
    run_uid: Uuid,
    item_key: &str,
) -> TestResult {
    let admission = repository
        .admit_ready_attempts(config, 1, Utc::now())
        .await?;
    let admitted = admission
        .admitted
        .into_iter()
        .find(|item| item.run_uid == run_uid)
        .expect("the only ready task must be admitted");
    let fence = TaskAttemptFence {
        tenant_id: admitted.tenant_id,
        run_uid: admitted.run_uid,
        task_id: admitted.task_id,
        controller_generation: admitted.controller_generation,
        attempt_generation: admitted.attempt_generation,
        dispatch_uid: admitted.dispatch_uid,
        capacity_reservation_uid: admitted.capacity_reservation_uid,
        watchdog_trigger_uid: admitted.watchdog_trigger_uid,
        attempt_deadline_at: admitted.attempt_deadline_at,
    };
    assert!(matches!(
        repository.start_task_attempt(fence).await?,
        TaskAttemptStartOutcome::Started(_)
    ));
    let outcome = completed_task_outcome(
        json!({ "item_key": item_key }),
        ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
    );
    assert!(matches!(
        repository
            .settle_task_attempt(config, fence, outcome, None, Utc::now())
            .await?,
        moa_execution::repository::task::TaskAttemptSettlementOutcome::Applied { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn failed_root_cancels_join_without_rolling_back_later_sibling_settlement_db() -> TestResult {
    // Pins: once one root failure canonically cancels an unmaterialized join, the independent
    // sibling may still complete or fail without reopening the join or rolling back settlement.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();

    for sibling_fails in [false, true] {
        let key = if sibling_fails {
            "failed-root-then-failed-sibling"
        } else {
            "failed-root-then-completed-sibling"
        };
        let mut candidate = new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(3));
        let mut join = output_node("join");
        join.depends_on = vec!["root-a".to_string(), "root-b".to_string()];
        candidate.plan.definition.nodes = vec![output_node("root-a"), output_node("root-b"), join];
        candidate.plan.estimate.tasks = 3;
        let run = create_run(&repository, scope, candidate).await?;
        repository
            .initialize_scheduler_state(scope, run.run_uid)
            .await?;
        let root_tasks = [
            logical_task(run.run_uid, "root-a", "", estimate(1)),
            logical_task(run.run_uid, "root-b", "", estimate(1)),
        ];
        for task in &root_tasks {
            assert!(matches!(
                repository
                    .materialize_ready_page(
                        scope,
                        &config,
                        ReadyMaterializationRequest {
                            run_uid: run.run_uid,
                            plan_revision: 1,
                            node_id: task.node_id.clone(),
                            expected_cursor: 0,
                            reduce_cursor: None,
                            source_exhausted: true,
                            terminal_output: None,
                            condition_skipped: false,
                            tasks: vec![task.clone()],
                        },
                    )
                    .await?,
                ReadyMaterializationOutcome::Applied { next_cursor: 1, .. }
            ));
        }

        let admitted = repository
            .admit_ready_attempts(&config, 2, Utc::now())
            .await?
            .admitted;
        assert_eq!(admitted.len(), 2, "both independent roots must be admitted");
        let fence_for = |item: &ExecutionAdmissionItem| TaskAttemptFence {
            tenant_id: item.tenant_id,
            run_uid: item.run_uid,
            task_id: item.task_id,
            controller_generation: item.controller_generation,
            attempt_generation: item.attempt_generation,
            dispatch_uid: item.dispatch_uid,
            capacity_reservation_uid: item.capacity_reservation_uid,
            watchdog_trigger_uid: item.watchdog_trigger_uid,
            attempt_deadline_at: item.attempt_deadline_at,
        };
        let failed_fence = fence_for(
            admitted
                .iter()
                .find(|item| item.task_id == root_tasks[0].task_id)
                .expect("root-a admission"),
        );
        let sibling_fence = fence_for(
            admitted
                .iter()
                .find(|item| item.task_id == root_tasks[1].task_id)
                .expect("root-b admission"),
        );
        for fence in [failed_fence, sibling_fence] {
            assert!(matches!(
                repository.start_task_attempt(fence).await?,
                TaskAttemptStartOutcome::Started(_)
            ));
        }

        let failed = ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage(0),
            result: ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                message: "root failed".to_string(),
            },
        };
        assert!(matches!(
            repository
                .settle_task_attempt(&config, failed_fence, failed.clone(), None, Utc::now())
                .await?,
            moa_execution::repository::task::TaskAttemptSettlementOutcome::Applied { .. }
        ));

        let sibling_outcome = if sibling_fails {
            failed
        } else {
            completed_task_outcome(json!({ "root": "b" }), usage(0))
        };
        assert!(matches!(
            repository
                .settle_task_attempt(&config, sibling_fence, sibling_outcome, None, Utc::now(),)
                .await?,
            moa_execution::repository::task::TaskAttemptSettlementOutcome::Applied { .. }
        ));

        let join_projection = sqlx::query_as::<_, (String, i64, bool, bool, i64)>(
            "SELECT node_status, remaining_dependency_count, materialization_complete, \
                    aggregate_complete, total_task_count \
             FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='join'",
        )
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            join_projection,
            ("cancelled".to_string(), 0, true, true, 0),
            "the join remains one canonical unmaterialized cancellation"
        );
        let sibling = repository
            .load_task(scope, run.run_uid, root_tasks[1].task_id)
            .await?
            .expect("settled sibling task");
        assert_eq!(
            sibling.status,
            if sibling_fails {
                ExecutionTaskStatus::Failed
            } else {
                ExecutionTaskStatus::Completed
            },
            "the sibling settlement must commit independently"
        );
    }
    Ok(())
}

#[tokio::test]
async fn external_signal_settlement_preserves_newer_task_progress_db() -> TestResult {
    // Pins: resolving a storage-only signal must preserve a task projection timestamp that is
    // newer than the process-observed settlement time while still committing the exact outcome.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "external-signal-monotonic-progress",
        ExecutionRunStatus::Queued,
        budget(1),
    );
    candidate.plan.definition.nodes = vec![output_node("signal")];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    let mut signal = logical_task(run.run_uid, "signal", "", estimate(1));
    signal.kind = LogicalTaskKind::WaitSignal {
        signal_name: "upstream-ready".to_string(),
        wait_policy: ExecutionWaitPolicy {
            expiry: ExecutionTemporalTarget::After { delay_seconds: 180 },
            on_expiry: ExecutionWaitExpiryAction::FailTask,
        },
    };
    let ReadyMaterializationOutcome::Applied { tasks, .. } = repository
        .materialize_ready_page(
            scope,
            &ExecutionConfig::default(),
            ReadyMaterializationRequest {
                run_uid: run.run_uid,
                plan_revision: 1,
                node_id: "signal".to_string(),
                expected_cursor: 0,
                reduce_cursor: None,
                source_exhausted: true,
                terminal_output: None,
                condition_skipped: false,
                tasks: vec![signal],
            },
        )
        .await?
    else {
        panic!("fresh signal wait must materialize");
    };
    let task = &tasks[0];
    let future_progress_at: chrono::DateTime<Utc> = sqlx::query_scalar(
        "UPDATE moa.execution_task SET last_progress_at=NOW() + INTERVAL '1 minute' \
         WHERE run_uid=$1 AND task_id=$2 RETURNING last_progress_at",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET last_progress_at=$2 \
         WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .bind(future_progress_at)
    .execute(&pool)
    .await?;

    let TaskOutcomeWrite::Applied { task: settled, .. } = repository
        .complete_external_wait(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            task.task_id,
            task.generation,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(0),
                result: ExecutionTaskResult::Completed {
                    output: json!({"signal": "accepted"}),
                    citations: Vec::new(),
                },
            },
        )
        .await?
    else {
        panic!("exact external signal settlement must apply");
    };
    assert_eq!(settled.status, ExecutionTaskStatus::Completed);
    assert_eq!(settled.output, Some(json!({"signal": "accepted"})));
    assert_eq!(settled.last_progress_at, future_progress_at);
    let settled_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("settled signal run must remain visible");
    assert_eq!(settled_run.last_progress_at, future_progress_at);
    Ok(())
}

#[tokio::test]
async fn relative_timer_is_parked_once_and_stale_delivery_is_fenced_db() -> TestResult {
    // Pins: timer, review, and signal waits atomically persist exact absolute reasons and delayed
    // deliveries; their compact run projection keeps deterministic phase precedence and earliest
    // wake without scanning task rows, and stale delivery remains generation-fenced.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "relative-wait",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![
        output_node("timer"),
        output_node("review"),
        output_node("signal"),
    ];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    let mut task = logical_task(run.run_uid, "timer", "", estimate(1));
    task.kind = LogicalTaskKind::WaitUntil {
        wake: ExecutionTemporalTarget::After { delay_seconds: 60 },
        result: json!({ "elapsed": true }),
    };
    let ReadyMaterializationOutcome::Applied {
        tasks,
        triggers,
        next_cursor,
    } = repository
        .materialize_ready_page(
            scope,
            &ExecutionConfig::default(),
            ReadyMaterializationRequest {
                run_uid: run.run_uid,
                plan_revision: 1,
                node_id: "timer".to_string(),
                expected_cursor: 0,
                reduce_cursor: None,
                source_exhausted: true,
                terminal_output: None,
                condition_skipped: false,
                tasks: vec![task],
            },
        )
        .await?
    else {
        panic!("fresh timer materialization must apply");
    };
    assert_eq!(next_cursor, 1);
    assert_eq!(tasks[0].status, ExecutionTaskStatus::WaitingTimer);
    let waiting_since = tasks[0]
        .waiting_since
        .expect("timer must persist its wait-entry anchor");
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].due_at, waiting_since + Duration::seconds(60));
    let timer_task_id = tasks[0].task_id;
    let timer_trigger = triggers[0].clone();

    let waits = [
        (
            "review",
            LogicalTaskKind::Review {
                prompt: "approve durable work".to_string(),
                wait_policy: ExecutionWaitPolicy {
                    expiry: ExecutionTemporalTarget::After { delay_seconds: 120 },
                    on_expiry: ExecutionWaitExpiryAction::FailTask,
                },
            },
        ),
        (
            "signal",
            LogicalTaskKind::WaitSignal {
                signal_name: "upstream-ready".to_string(),
                wait_policy: ExecutionWaitPolicy {
                    expiry: ExecutionTemporalTarget::After { delay_seconds: 180 },
                    on_expiry: ExecutionWaitExpiryAction::FailTask,
                },
            },
        ),
    ];
    let mut exact_waits = Vec::new();
    for (node_id, kind) in waits {
        let mut task = logical_task(run.run_uid, node_id, "", estimate(1));
        task.kind = kind;
        let ReadyMaterializationOutcome::Applied {
            tasks, triggers, ..
        } = repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: node_id.to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![task],
                },
            )
            .await?
        else {
            panic!("fresh {node_id} wait must materialize");
        };
        assert_eq!(tasks.len(), 1);
        assert_eq!(triggers.len(), 1);
        exact_waits.push((node_id, tasks[0].task_id, triggers[0].due_at));
    }
    let run_state = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run must remain visible");
    assert_eq!(run_state.ready_task_count, 0);
    assert_eq!(run_state.status, ExecutionRunStatus::WaitingReview);
    assert_eq!(run_state.waiting_since, Some(waiting_since));
    assert_eq!(run_state.next_wake_at, Some(timer_trigger.due_at));
    assert_eq!(run_state.waiting_task_count, 3);
    assert_eq!(run_state.waiting_review_task_count, 1);
    assert_eq!(run_state.waiting_signal_task_count, 1);
    assert_eq!(run_state.waiting_timer_task_count, 1);
    assert!(!run_state.waiting_reasons_truncated);
    assert_eq!(run_state.waiting_reasons.len(), 3);
    assert!(run_state.waiting_reasons.iter().any(|reason| matches!(
        reason,
        moa_execution::state::WaitingReason::Timer {
            task_id,
            wake: ExecutionTemporalTarget::At { at },
        } if *task_id == timer_task_id && *at == timer_trigger.due_at
    )));
    assert!(run_state.waiting_reasons.iter().any(|reason| matches!(
        reason,
        moa_execution::state::WaitingReason::Review {
            task_id,
            wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::At { at },
                ..
            },
            ..
        } if *task_id == exact_waits[0].1 && *at == exact_waits[0].2
    )));
    assert!(run_state.waiting_reasons.iter().any(|reason| matches!(
        reason,
        moa_execution::state::WaitingReason::Signal {
            task_id,
            wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::At { at },
                ..
            },
            ..
        } if *task_id == exact_waits[1].1 && *at == exact_waits[1].2
    )));

    sqlx::query(
        "UPDATE moa.execution_run SET controller_generation = controller_generation + 1 \
         WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    let (outcome, activation) = repository
        .fire_wait_trigger(
            scope,
            &ExecutionConfig::default(),
            timer_trigger.trigger_uid,
        )
        .await?;
    assert_eq!(
        outcome,
        TransitionOutcome::Rejected(TransitionRejection::InvalidTaskStatus)
    );
    assert!(activation.is_none());
    Ok(())
}
