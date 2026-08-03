//! Budget reservation and durable task-materialization persistence contracts.

use super::support::*;

#[tokio::test]
async fn aggregate_materialization_marker_applies_once_including_empty_map_db() -> TestResult {
    // Pins: the immutable node marker, not task insertion, is first-application evidence.
    // Empty maps and reducers apply once, exact retries replay, conflicts do not mutate, and a
    // transaction that fails after marker insertion rolls the marker back.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "aggregate-materialization",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    let empty_map = ExecutionNodeMaterialization::Map {
        node_id: "empty-map".to_string(),
        fanout_items: 0,
    };
    let MaterializationOutcome::Applied(applied) = repository
        .materialize_node(scope, run.run_uid, 1, Some(empty_map.clone()), Vec::new())
        .await?
    else {
        panic!("empty map marker must first apply");
    };
    assert_eq!(applied.marker, Some(empty_map.clone()));
    assert!(applied.tasks.is_empty());
    assert!(applied.inserted_task_ids.is_empty());
    assert_eq!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(empty_map), Vec::new())
            .await?,
        MaterializationOutcome::Replayed { tasks: Vec::new() }
    );
    assert_eq!(
        repository
            .materialize_node(
                scope,
                run.run_uid,
                1,
                Some(ExecutionNodeMaterialization::Map {
                    node_id: "empty-map".to_string(),
                    fanout_items: 1,
                }),
                Vec::new(),
            )
            .await?,
        MaterializationOutcome::Conflict
    );

    let reducer = ExecutionNodeMaterialization::Reduce {
        node_id: "reduce".to_string(),
        reducer_depth: 3,
    };
    assert!(matches!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(reducer.clone()), Vec::new())
            .await?,
        MaterializationOutcome::Applied(_)
    ));
    assert_eq!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(reducer), Vec::new())
            .await?,
        MaterializationOutcome::Replayed { tasks: Vec::new() }
    );

    let rollback_marker = ExecutionNodeMaterialization::Map {
        node_id: "rollback-map".to_string(),
        fanout_items: 1,
    };
    let mut invalid_task = logical_task(
        run.run_uid,
        "rollback-map",
        "one",
        ExecutionEstimate {
            cost_microusd: 1,
            tokens: 1,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 1,
        },
    );
    invalid_task.generation = 2;
    assert!(
        repository
            .materialize_node(
                scope,
                run.run_uid,
                1,
                Some(rollback_marker.clone()),
                vec![invalid_task],
            )
            .await
            .is_err()
    );
    assert!(matches!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(rollback_marker), Vec::new())
            .await?,
        MaterializationOutcome::Applied(_)
    ));
    let marker_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_node_materialization WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(marker_count, 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_concurrent_reservations_never_exceed_budget_db() -> TestResult {
    // Pins: concurrent task reservations lock the run ledger and admit exactly the approved count.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut limit = budget(7);
    limit.max_cost_microusd = Some(14);
    limit.max_tokens = Some(21);
    limit.max_tool_calls = Some(7);
    limit.max_retrieved_bytes = Some(28);
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "reservation-race",
            ExecutionRunStatus::Queued,
            limit,
        ),
    )
    .await?;
    let tasks = (0..100)
        .map(|index| {
            logical_task(
                run.run_uid,
                "screen",
                &format!("company-{index:03}"),
                ExecutionEstimate {
                    cost_microusd: 2,
                    tokens: 3,
                    tasks: 1,
                    tool_calls: 1,
                    retrieved_bytes: 4,
                },
            )
        })
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
        .await?;

    let mut joins = JoinSet::new();
    for task in tasks {
        let repository = repository.clone();
        joins.spawn(async move {
            repository
                .reserve_task(scope, run.run_uid, task.task_id, 1)
                .await
        });
    }
    let mut reserved = 0;
    let mut rejected = 0;
    while let Some(result) = joins.join_next().await {
        match result?? {
            ReservationOutcome::Reserved(_) => reserved += 1,
            ReservationOutcome::Terminalized(terminalized)
                if terminalized.rejection == ReservationRejection::BudgetExceeded =>
            {
                rejected += 1;
            }
            other => panic!("unexpected concurrent reservation result: {other:?}"),
        }
    }
    assert_eq!(reserved, 7);
    assert_eq!(rejected, 93);
    let loaded = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(
        loaded.reserved,
        ExecutionEstimate {
            cost_microusd: 14,
            tokens: 21,
            tasks: 7,
            tool_calls: 7,
            retrieved_bytes: 28,
        }
    );
    Ok(())
}

#[tokio::test]
async fn reservation_near_bigint_limit_returns_budget_exceeded_db() -> TestResult {
    // Pins: valid near-BIGINT ledgers reject over-budget work without PostgreSQL arithmetic overflow.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let maximum = i64::MAX as u64;
    let approved = ExecutionBudgetLimit {
        max_cost_microusd: Some(maximum),
        max_tokens: Some(maximum),
        max_tasks: Some(maximum),
        max_tool_calls: Some(maximum),
        max_retrieved_bytes: Some(maximum),
        deadline_at: Some(pg_deadline(Duration::hours(1))),
    };
    let cases = [
        (
            "cost",
            "UPDATE moa.execution_run SET consumed_cost_microusd = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 1,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tokens",
            "UPDATE moa.execution_run SET consumed_tokens = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 1,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tasks",
            "UPDATE moa.execution_run SET consumed_tasks = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tool-calls",
            "UPDATE moa.execution_run SET consumed_tool_calls = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 1,
                retrieved_bytes: 0,
            },
        ),
        (
            "retrieved-bytes",
            "UPDATE moa.execution_run SET consumed_retrieved_bytes = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 1,
            },
        ),
    ];

    for (dimension, fixture_sql, reservation) in cases {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("bigint-{dimension}"),
                ExecutionRunStatus::Queued,
                approved.clone(),
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, dimension, "", reservation);
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        assert_eq!(
            sqlx::query(fixture_sql)
                .bind(run.run_uid)
                .execute(test_db.store().pool())
                .await?
                .rows_affected(),
            1,
            "fixture must place {dimension} at BIGINT max"
        );
        let reservation = repository
            .reserve_task(scope, run.run_uid, task.task_id, 1)
            .await?;
        assert!(
            matches!(
                &reservation,
                ReservationOutcome::Terminalized(terminalized)
                    if terminalized.rejection == ReservationRejection::BudgetExceeded
            ),
            "{dimension} overflow must be classified as budget exhaustion: {reservation:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn reservation_budget_or_deadline_rejection_consumes_zero_task_units_db() -> TestResult {
    // Pins: one reservation-admission transaction that loses to an elapsed
    // deadline or exhausted budget records the exact typed failure under its
    // current generation, leaves admission usage unconsumed, and wakes the run
    // without a second repository call.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let cases = [
        (
            "deadline",
            ExecutionBudgetLimit {
                deadline_at: Some(moa_test_support::fixtures::pg_now() - Duration::seconds(1)),
                ..budget(1)
            },
            ReservationRejection::DeadlineElapsed,
            moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
        ),
        (
            "budget",
            ExecutionBudgetLimit {
                max_tasks: Some(0),
                ..budget(1)
            },
            ReservationRejection::BudgetExceeded,
            moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded,
        ),
    ];

    for (name, approved_budget, rejection, failure_class) in cases {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("reservation-terminal-{name}"),
                ExecutionRunStatus::Queued,
                approved_budget,
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, name, "", estimate(1));
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        let before_rejection = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible before reservation admission");
        let admission = repository
            .reserve_task(scope, run.run_uid, task.task_id, task.generation)
            .await?;
        assert!(
            matches!(
                &admission,
                ReservationOutcome::Terminalized(terminalized)
                    if terminalized.rejection == rejection
            ),
            "{name} admission must return its committed typed terminal result: {admission:?}"
        );
        let outcome = ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage(0),
            result: ExecutionTaskResult::Failed {
                class: failure_class.clone(),
                message: format!("execution task reservation rejected: {rejection:?}"),
            },
        };

        let persisted = repository
            .load_task(scope, run.run_uid, task.task_id)
            .await?
            .expect("reservation rejection must leave a queryable terminal task");
        assert_eq!(persisted.status, ExecutionTaskStatus::Failed);
        assert_eq!(persisted.current_outcome, Some(outcome));
        assert_eq!(persisted.generation, 1);
        assert_eq!(persisted.reserved, ExecutionEstimate::default());
        assert_eq!(persisted.actual, usage(0));
        assert_eq!(persisted.actual_tasks, 0);
        assert_eq!(persisted.reserved_at, None);
        assert_eq!(persisted.started_at, None);
        assert_eq!(persisted.outcome_audit.len(), 1);
        assert_eq!(
            persisted.outcome_audit[0]
                .get("accepted")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let after_rejection = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible after reservation rejection");
        assert_eq!(after_rejection.reserved, ExecutionEstimate::default());
        assert_eq!(after_rejection.consumed, ExecutionEstimate::default());
        assert!(!after_rejection.budget_overrun);
        assert_eq!(after_rejection.progress_failed_tasks, 1);
        assert_eq!(after_rejection.wake_epoch, before_rejection.wake_epoch + 1);

        let replay = repository
            .reserve_task(scope, run.run_uid, task.task_id, task.generation)
            .await?;
        assert!(
            matches!(
                &replay,
                ReservationOutcome::AlreadyTerminalized(terminalized)
                    if terminalized.rejection == rejection
                        && terminalized.task == persisted
                        && terminalized.run == after_rejection
            ),
            "exact replay must return the committed typed result: {replay:?}"
        );
        assert_eq!(
            repository
                .load_run(scope, run.run_uid)
                .await?
                .expect("run remains visible after replay"),
            after_rejection,
            "exact replay must not repeat accounting or wake advancement"
        );
        assert_eq!(
            repository
                .reserve_task(scope, run.run_uid, task.task_id, task.generation + 1)
                .await?,
            ReservationOutcome::Rejected(ReservationRejection::GenerationMismatch),
            "a terminalized admission must retain its generation fence"
        );
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_task_materialization_is_exact_and_non_mutating_db() -> TestResult {
    // Pins: a batch containing exact replay, new work, and semantic drift must
    // reject atomically without retaining the new row or changing run progress.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "materialization",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "lookup", "AAPL", estimate(1));
    let first = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    let replay = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    assert_eq!(replay, first);

    let new_task = logical_task(run.run_uid, "lookup", "GOOG", estimate(1));
    let mut drifted = task.clone();
    drifted.input = json!({ "company": "MSFT" });
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![task.clone(), new_task.clone(), drifted],
        )
        .await
        .expect_err("mixed replay, insertion, and semantic drift must reject atomically");
    assert_eq!(
        repository
            .load_task(scope, run.run_uid, new_task.task_id)
            .await?,
        None,
        "the new row from the rejected batch must roll back"
    );
    assert_eq!(
        repository
            .load_task(scope, run.run_uid, task.task_id)
            .await?,
        Some(first[0].clone()),
        "the first-write task must remain byte-exact after rejected drift"
    );
    let loaded = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(loaded.progress_total_tasks, 1);
    Ok(())
}

#[tokio::test]
async fn ten_thousand_tasks_materialize_cancel_and_replay_without_residual_reservations_db()
-> TestResult {
    // Pins: the configured maximum 10,000-task fanout applies and replays set-wise,
    // then one cancellation terminalizes every generation, reconciles exact task
    // counters, releases every reservation, and remains non-mutating on replay.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "ten-thousand-task-batch",
            ExecutionRunStatus::Queued,
            budget(10_000),
        ),
    )
    .await?;
    let tasks = (0..10_000_u64)
        .map(|index| logical_task(run.run_uid, "fanout", &format!("{index:05}"), estimate(1)))
        .collect::<Vec<_>>();

    let first = repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
        .await?;
    assert_eq!(first.len(), 10_000);
    assert_eq!(
        first
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>(),
        tasks
            .iter()
            .map(|task| task.item_key.as_str())
            .collect::<Vec<_>>()
    );
    let first_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("10,000-task run remains visible after first materialization");
    assert_eq!(first_run.progress_total_tasks, 10_000);

    let replay = repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
        .await?;
    assert_eq!(replay, first);
    assert_eq!(
        repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("10,000-task run remains visible after replay"),
        first_run,
        "exact materialization replay must not mutate run progress or wake state"
    );

    let mut reservation_setup = pool.begin().await?;
    let reserved_tasks = sqlx::query(
        "UPDATE moa.execution_task \
         SET status = 'reserved', \
             reserved_cost_microusd = estimate_cost_microusd, \
             reserved_tokens = estimate_tokens, \
             reserved_tasks = estimate_tasks, \
             reserved_tool_calls = estimate_tool_calls, \
             reserved_retrieved_bytes = estimate_retrieved_bytes, \
             reserved_at = NOW(), updated_at = NOW() \
         WHERE task_id IN ( \
             SELECT task_id FROM moa.execution_task \
             WHERE run_uid = $1 AND status = 'pending' \
             ORDER BY task_id LIMIT 100 \
         )",
    )
    .bind(run.run_uid)
    .execute(&mut *reservation_setup)
    .await?;
    assert_eq!(reserved_tasks.rows_affected(), 100);
    assert_eq!(
        sqlx::query(
            "UPDATE moa.execution_run \
             SET reserved_cost_microusd = 100, reserved_tokens = 100, \
                 reserved_tasks = 100, reserved_tool_calls = 100, \
                 reserved_retrieved_bytes = 100, updated_at = NOW() \
             WHERE run_uid = $1"
        )
        .bind(run.run_uid)
        .execute(&mut *reservation_setup)
        .await?
        .rows_affected(),
        1
    );
    reservation_setup.commit().await?;
    assert_eq!(
        repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("10,000-task run remains visible after reservation setup")
            .reserved,
        ExecutionEstimate {
            cost_microusd: 100,
            tokens: 100,
            tasks: 100,
            tool_calls: 100,
            retrieved_bytes: 100,
        }
    );

    let reason = "cancel 10,000-task fanout".to_string();
    let request = cancellation_request(&repository, scope, run.run_uid, reason.clone()).await?;
    let CancellationOutcome::Cancelled {
        commit: cancelled,
        metrics,
    } = repository.cancel_run(scope, run.run_uid, request).await?
    else {
        panic!("10,000-task cancellation must commit");
    };
    assert_eq!(cancelled.task_ids_to_release.len(), 10_000);
    assert_eq!(metrics.tasks.len(), 10_000);
    assert_eq!(
        metrics
            .tasks
            .iter()
            .filter(|transition| transition.prior_status == ExecutionTaskStatus::Reserved)
            .count(),
        100
    );
    assert_eq!(
        metrics
            .tasks
            .iter()
            .filter(|transition| transition.prior_status == ExecutionTaskStatus::Pending)
            .count(),
        9_900
    );
    assert!(
        metrics
            .tasks
            .iter()
            .all(|transition| transition.status == ExecutionTaskStatus::Cancelled)
    );
    assert_eq!(cancelled.run.progress_total_tasks, 10_000);
    assert_eq!(cancelled.run.progress_cancelled_tasks, 10_000);
    assert_eq!(cancelled.run.consumed.tasks, 10_000);
    assert_eq!(cancelled.run.reserved, ExecutionEstimate::default());

    let mut cursor = None;
    let mut terminal_count = 0_usize;
    loop {
        let page = repository
            .list_tasks(
                scope,
                run.run_uid,
                ExecutionTaskPageRequest {
                    limit: 1_000,
                    cursor,
                },
            )
            .await?;
        terminal_count += page.tasks.len();
        assert!(page.tasks.iter().all(|task| {
            task.status == ExecutionTaskStatus::Cancelled
                && task.reserved == ExecutionEstimate::default()
                && task.actual_tasks == 1
                && matches!(
                    task.current_outcome.as_ref().map(|outcome| &outcome.result),
                    Some(ExecutionTaskResult::Cancelled { reason: actual }) if actual == &reason
                )
        }));
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        cursor = Some(next_cursor);
    }
    assert_eq!(terminal_count, 10_000);

    let cancelled_snapshot = cancelled.run.clone();
    let cancelled_release_ids = cancelled.task_ids_to_release.clone();
    let replay_request = cancellation_request(&repository, scope, run.run_uid, reason).await?;
    let CancellationOutcome::Replayed(replayed) = repository
        .cancel_run(scope, run.run_uid, replay_request)
        .await?
    else {
        panic!("exact 10,000-task cancellation replay must recover the first commit");
    };
    assert_eq!(replayed.run, cancelled_snapshot);
    assert_eq!(replayed.task_ids_to_release, cancelled_release_ids);
    Ok(())
}
