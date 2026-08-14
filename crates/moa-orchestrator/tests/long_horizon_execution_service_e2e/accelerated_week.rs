//! Accelerated eight-logical-day wait and restart scenario.

use super::*;

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn accelerated_eight_day_waits_survive_repeated_process_and_valkey_restarts_service_e2e()
-> Result<()> {
    // Pins: timer, review-expiry, and signal-expiry waits persist exact due order,
    // release active compute, and resume once across repeated process/cache loss.
    let fixture = execution_fixture(vec![(
        "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
        "1".to_string(),
    )])
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "accelerated-week",
        vec![
            node(
                "day-two-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: after_logical_days(2),
                    result: json!({"day": 2}),
                },
                json!({"type": "object"}),
            ),
            node(
                "day-five-review",
                &["day-two-timer"],
                ExecutionOperation::Review {
                    prompt: "approve the logical-day-five checkpoint".to_string(),
                    wait_policy: continue_wait(3, json!({"approved_by_expiry": true})),
                },
                json!({"type": "object"}),
            ),
            node(
                "day-eight-signal",
                &["day-five-review"],
                ExecutionOperation::WaitSignal {
                    signal_name: "logical-week-close".to_string(),
                    wait_policy: continue_wait(3, json!({"closed_by_expiry": true})),
                },
                json!({"type": "object"}),
            ),
            output_node(&["day-eight-signal"], json!({"week": "complete"})),
        ],
        Duration::from_secs(40),
    )
    .await?;

    if let Err(error) = await_run_status(&test, &run, ExecutionRunStatus::WaitingTimer).await {
        let run_state: (String, String, i64, i64, i64) = sqlx::query_as(
            "SELECT status, activation_state, controller_generation, wake_epoch, \
                    processed_wake_epoch FROM moa.execution_run WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
        let dispatches: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
            "SELECT dispatch_kind, state, delivery_attempts, last_error \
             FROM moa.execution_dispatch_outbox WHERE run_uid=$1 ORDER BY created_at",
        )
        .bind(run.run_uid)
        .fetch_all(&pool)
        .await?;
        let nodes: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                 'node_id', node_id, 'status', node_status, \
                 'cursor', materialization_cursor, \
                 'remaining_dependencies', remaining_dependency_count) \
                 ORDER BY node_order), '[]'::jsonb) \
             FROM moa.execution_node_state WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
        let controller_invocations = restate_rows(
            &fixture,
            &format!(
                "SELECT * FROM sys_invocation WHERE target_service_name = \
                 'ExecutionRunController' AND target_service_key = '{}'",
                run.run_uid
            ),
        )
        .await
        .unwrap_or_default();
        let controller_journal = restate_rows(
            &fixture,
            &format!(
                "SELECT * FROM sys_journal WHERE id IN (SELECT id FROM sys_invocation \
                 WHERE target_service_name = 'ExecutionRunController' \
                   AND target_service_key = '{}')",
                run.run_uid
            ),
        )
        .await
        .unwrap_or_default();
        bail!(
            "{error:#}; run_state={run_state:?}; dispatches={dispatches:?}; nodes={nodes}; \
             controller_invocations={controller_invocations:?}; \
             controller_journal={controller_journal:?}"
        );
    }
    await_task_status(
        &test,
        &run,
        "day-two-timer",
        ExecutionTaskStatus::WaitingTimer,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    fixture.hard_crash_and_restart_orchestrator().await?;
    fixture.recreate_valkey_after_loss().await?;

    await_run_status(&test, &run, ExecutionRunStatus::WaitingReview).await?;
    await_task_status(
        &test,
        &run,
        "day-five-review",
        ExecutionTaskStatus::WaitingReview,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    fixture.restart_orchestrator().await?;
    fixture.stop_valkey().await?;
    fixture.restart_valkey().await?;

    await_run_status(&test, &run, ExecutionRunStatus::WaitingSignal).await?;
    await_task_status(
        &test,
        &run,
        "day-eight-signal",
        ExecutionTaskStatus::WaitingSignal,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    fixture.hard_crash_and_restart_orchestrator().await?;

    let terminal = match await_run_status(&test, &run, ExecutionRunStatus::Completed).await {
        Ok(terminal) => terminal,
        Err(error) => {
            let run_state: Value = sqlx::query_scalar(
                "SELECT jsonb_build_object( \
                    'status', status, 'activation_state', activation_state, \
                    'controller_generation', controller_generation, 'wake_epoch', wake_epoch, \
                    'processed_wake_epoch', processed_wake_epoch, \
                    'ready_task_count', ready_task_count, \
                    'active_task_count', active_task_count, \
                    'waiting_task_count', waiting_task_count, \
                    'pending_terminal_status', pending_terminal_status) \
                 FROM moa.execution_run WHERE run_uid=$1",
            )
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
            let dispatches: Value = sqlx::query_scalar(
                "SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                    'dispatch_uid', dispatch_uid, 'kind', dispatch_kind, 'state', state, \
                    'delivery_attempts', delivery_attempts, 'last_error', last_error) \
                    ORDER BY created_at), '[]'::jsonb) \
                 FROM moa.execution_dispatch_outbox WHERE run_uid=$1",
            )
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
            let nodes: Value = sqlx::query_scalar(
                "SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                    'node_id', node_id, 'status', node_status, \
                    'remaining_dependencies', remaining_dependency_count, \
                    'ready_tasks', ready_task_count, 'active_tasks', active_task_count, \
                    'waiting_tasks', waiting_task_count, 'terminal_tasks', terminal_task_count) \
                    ORDER BY node_order), '[]'::jsonb) \
                 FROM moa.execution_node_state WHERE run_uid=$1",
            )
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
            let tasks: Value = sqlx::query_scalar(
                "SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                    'node_id', node_id, 'status', status, 'attempt_state', attempt_state, \
                    'generation', generation, 'attempt_generation', attempt_generation, \
                    'active_dispatch_uid', active_dispatch_uid) ORDER BY created_at), '[]'::jsonb) \
                 FROM moa.execution_task WHERE run_uid=$1",
            )
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
            let attempt_dispatches: Vec<Uuid> = sqlx::query_scalar(
                "SELECT dispatch_uid FROM moa.execution_dispatch_outbox \
                 WHERE run_uid=$1 AND dispatch_kind='task_attempt'",
            )
            .bind(run.run_uid)
            .fetch_all(&pool)
            .await?;
            let attempt_keys = attempt_dispatches
                .iter()
                .map(|dispatch_uid| format!("'{dispatch_uid}'"))
                .collect::<Vec<_>>()
                .join(", ");
            let attempt_filter = if attempt_keys.is_empty() {
                "false".to_string()
            } else {
                format!(
                    "target_service_name = 'ExecutionTaskAttempt' AND \
                     target_service_key IN ({attempt_keys})"
                )
            };
            let invocation_filter = format!(
                "(target_service_name = 'ExecutionRunController' AND target_service_key = '{}') \
                 OR ({attempt_filter}) OR (target_service_name = 'ToolExecutor' AND \
                 invoked_by_id IN (SELECT id FROM sys_invocation WHERE {attempt_filter}))",
                run.run_uid
            );
            let invocations = restate_rows(
                &fixture,
                &format!(
                    "SELECT id, status, target_service_name, target_service_key \
                     FROM sys_invocation WHERE {invocation_filter}"
                ),
            )
            .await
            .unwrap_or_default();
            let journal = restate_rows(
                &fixture,
                &format!(
                    "SELECT id, index, entry_type, name, entry_json FROM sys_journal \
                     WHERE id IN (SELECT id FROM sys_invocation WHERE {invocation_filter}) \
                       AND entry_type IN ('Command: Output', 'Notification: Call') \
                     ORDER BY id, index"
                ),
            )
            .await
            .unwrap_or_default();
            bail!(
                "{error:#}; run_state={run_state}; dispatches={dispatches}; nodes={nodes}; \
                 tasks={tasks}; invocations={invocations:?}; journal={journal:?}"
            );
        }
    };
    assert_eq!(terminal.output, Some(json!({"week": "complete"})));
    let trigger_rows = sqlx::query(
        "SELECT task.node_id, trigger.trigger_kind, trigger.created_at, \
                trigger.due_at, trigger.delivered_at, trigger.state, \
                trigger.attempt_generation \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_task AS task \
           ON task.run_uid = trigger.run_uid AND task.task_id = trigger.task_id \
         WHERE trigger.run_uid = $1 \
           AND trigger.trigger_kind IN ('task_timer', 'wait_expiry')",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(trigger_rows.len(), 3);
    let mut observed = std::collections::BTreeMap::new();
    for row in trigger_rows {
        let node_id: String = row.try_get("node_id")?;
        assert_eq!(row.try_get::<String, _>("state")?, "delivered");
        assert_eq!(
            row.try_get::<Option<i64>, _>("attempt_generation")?,
            Some(1)
        );
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let due_at: DateTime<Utc> = row.try_get("due_at")?;
        let delivered_at: DateTime<Utc> = row
            .try_get::<Option<DateTime<Utc>>, _>("delivered_at")?
            .with_context(|| format!("{node_id} trigger omitted delivered_at"))?;
        observed.insert(
            node_id,
            (
                row.try_get::<String, _>("trigger_kind")?,
                due_at.signed_duration_since(created_at),
                delivered_at,
            ),
        );
    }
    let timer = observed
        .get("day-two-timer")
        .context("day-two timer trigger missing")?;
    let review = observed
        .get("day-five-review")
        .context("day-five review trigger missing")?;
    let signal = observed
        .get("day-eight-signal")
        .context("day-eight signal trigger missing")?;
    assert_eq!(timer.0, "task_timer");
    assert_eq!(review.0, "wait_expiry");
    assert_eq!(signal.0, "wait_expiry");
    let within = |actual: TimeDelta, expected: TimeDelta| {
        (actual - expected).num_milliseconds().unsigned_abs() <= 250
    };
    assert!(
        within(timer.1, TimeDelta::seconds(4)),
        "timer due={:?}",
        timer.1
    );
    assert!(
        within(review.1, TimeDelta::seconds(6)),
        "review due={:?}",
        review.1
    );
    assert!(
        within(signal.1, TimeDelta::seconds(6)),
        "signal due={:?}",
        signal.1
    );
    assert!(timer.2 < review.2 && review.2 < signal.2);
    Ok(())
}
