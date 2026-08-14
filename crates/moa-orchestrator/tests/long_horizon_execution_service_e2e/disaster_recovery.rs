//! Postgres, outbox, Restate-loss, and Valkey-loss recovery coverage.

use super::*;
use moa_core::types::tools::{AsyncToolJobCallbackOutcome, AsyncToolJobTerminalOutcome};

#[tokio::test]
#[ignore = "requires Docker for destructive empty-Restate recovery"]
async fn unbound_external_start_recovers_provider_job_without_replaying_attempt_service_e2e()
-> Result<()> {
    // Pins: provider start is committed under the pre-reserved key, then all Restate state is
    // lost before bind. The expired Unbound intent calls recover_start with the same identity,
    // binds and releases its owner without replaying TaskAttempt, and sparse reconciliation
    // completes the one provider effect.
    let fixture = external_job_execution_fixture(vec![
        (
            "MOA_EXECUTION_ACTIVE_ATTEMPT_TIMEOUT_SECONDS".to_string(),
            "15".to_string(),
        ),
        // Two constraints bracket this value. It must stay strictly below the attempt
        // timeout or the orchestrator refuses to boot, and it must outlast the provider
        // start this scenario performs: the start declares no bound of its own, so the
        // floor is its whole window, and a floor under it kills the attempt before the
        // start commits and leaves no provider job to recover.
        (
            "MOA_EXECUTION_ATTEMPT_HEARTBEAT_STALENESS_SECONDS".to_string(),
            "10".to_string(),
        ),
        (
            "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
            "1".to_string(),
        ),
    ])
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "external-start-total-restate-loss",
        vec![
            external_job_capability_node("external-job", json!({"value": "recover"})),
            output_node(&["external-job"], json!({"external_recovery": "complete"})),
        ],
        Duration::from_secs(55),
    )
    .await?;
    let controller = fixture
        .fixture_external_job()
        .context("external-job recovery fixture omitted provider controller")?;
    let starts = controller.wait_for_starts(1, SCENARIO_TIMEOUT).await?;
    let start = &starts[0];
    let before_loss = sqlx::query(
        "SELECT job.state AS job_state, job.idempotency_key, task.status AS task_status, \
                task.attempt_state, task.attempt_generation, dispatch.dispatch_uid, \
                dispatch.delivered_at, dispatch.delivery_attempts, recovery.trigger_uid, \
                recovery.due_at, recovery.state AS recovery_state \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_task AS task \
           ON task.run_uid = job.run_uid AND task.task_id = job.task_id \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.run_uid = task.run_uid AND dispatch.task_id = task.task_id \
          AND dispatch.dispatch_kind = 'task_attempt' \
          AND dispatch.attempt_generation = task.attempt_generation \
         JOIN moa.execution_trigger AS recovery \
           ON recovery.payload ->> 'external_job_uid' = job.external_job_uid::TEXT \
          AND (recovery.payload ->> 'job_generation')::BIGINT = job.job_generation \
          AND recovery.trigger_kind = 'external_start_recovery' \
         WHERE job.run_uid = $1 AND job.external_job_uid = $2",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(before_loss.try_get::<String, _>("job_state")?, "unbound");
    assert_eq!(
        before_loss.try_get::<String, _>("idempotency_key")?,
        start.context.idempotency_key
    );
    assert_eq!(before_loss.try_get::<String, _>("task_status")?, "running");
    assert_eq!(
        before_loss.try_get::<String, _>("attempt_state")?,
        "running"
    );
    assert_eq!(before_loss.try_get::<i64, _>("attempt_generation")?, 1);
    assert_eq!(
        before_loss.try_get::<String, _>("recovery_state")?,
        "pending"
    );
    let task_dispatch_uid: Uuid = before_loss.try_get("dispatch_uid")?;
    let task_delivered_at: DateTime<Utc> = before_loss
        .try_get::<Option<DateTime<Utc>>, _>("delivered_at")?
        .context("provider-start TaskAttempt omitted delivered_at")?;
    let recovery_trigger_uid: Uuid = before_loss.try_get("trigger_uid")?;

    fixture.recreate_restate_after_loss().await?;
    controller.queue_reconcile_outcomes([AsyncToolJobCallbackOutcome::Terminal {
        outcome: AsyncToolJobTerminalOutcome::Completed {
            output: json!({"provider": "recovered"}),
        },
    }]);
    fixture.hard_crash_and_restart_orchestrator().await?;
    // The provider committed its job before this gate. Release only after the
    // pre-loss handler is gone so it cannot bind and settle the recovery trigger.
    controller.release_starts(1);

    let recoveries = controller.wait_for_recoveries(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(recoveries.len(), 1);
    assert_eq!(recoveries[0].context, start.context);
    let completed = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(
        completed.output,
        Some(json!({"external_recovery": "complete"}))
    );
    let after_recovery = sqlx::query(
        "SELECT job.state AS job_state, job.provider_job_id, job.idempotency_key, \
                dispatch.state AS task_dispatch_state, dispatch.delivered_at, \
                dispatch.delivery_attempts, recovery.state AS recovery_state, \
                recovery_dispatch.state AS recovery_dispatch_state, \
                recovery_dispatch.delivery_attempts AS recovery_delivery_attempts, \
                (SELECT COUNT(*) FROM moa.execution_capacity_reservation AS capacity \
                 WHERE capacity.external_job_uid = job.external_job_uid \
                   AND capacity.resource_dimension = 'external_jobs' \
                   AND capacity.state = 'released') AS released_external_receipts \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_dispatch_outbox AS dispatch ON dispatch.dispatch_uid = $2 \
         JOIN moa.execution_trigger AS recovery ON recovery.trigger_uid = $3 \
         JOIN moa.execution_dispatch_outbox AS recovery_dispatch \
           ON recovery_dispatch.trigger_uid = recovery.trigger_uid \
          AND recovery_dispatch.dispatch_kind = 'trigger_delivery' \
         WHERE job.external_job_uid = $1",
    )
    .bind(start.context.external_job_uid)
    .bind(task_dispatch_uid)
    .bind(recovery_trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        after_recovery.try_get::<String, _>("job_state")?,
        "completed"
    );
    assert_eq!(
        after_recovery.try_get::<String, _>("provider_job_id")?,
        start.provider_job_id
    );
    assert_eq!(
        after_recovery.try_get::<String, _>("idempotency_key")?,
        start.context.idempotency_key
    );
    assert_eq!(
        after_recovery.try_get::<String, _>("task_dispatch_state")?,
        "delivered"
    );
    assert_eq!(
        after_recovery.try_get::<Option<DateTime<Utc>>, _>("delivered_at")?,
        Some(task_delivered_at)
    );
    assert_eq!(after_recovery.try_get::<i32, _>("delivery_attempts")?, 1);
    assert_eq!(
        after_recovery.try_get::<String, _>("recovery_state")?,
        "superseded"
    );
    assert_eq!(
        after_recovery.try_get::<String, _>("recovery_dispatch_state")?,
        "cancelled"
    );
    assert_eq!(
        after_recovery.try_get::<i32, _>("recovery_delivery_attempts")?,
        1
    );
    assert_eq!(
        after_recovery.try_get::<i64, _>("released_external_receipts")?,
        1
    );
    assert_eq!(controller.starts().len(), 1);
    assert_eq!(controller.recoveries().len(), 1);
    assert_eq!(controller.reconciliations().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for destructive disposable dependency recovery"]
async fn postgres_outbox_redrives_after_total_restate_and_valkey_loss_service_e2e() -> Result<()> {
    // Pins: a committed timer survives empty Restate-state replacement and Valkey loss via
    // generation-fenced exact-dispatch re-drive from PostgreSQL's authoritative outbox.
    let fixture = execution_fixture(vec![(
        "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
        "1".to_string(),
    )])
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "disaster-recovery",
        vec![
            node(
                "recovery-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: after_logical_days(5),
                    result: json!({"recovered": true}),
                },
                json!({"type": "object"}),
            ),
            output_node(&["recovery-timer"], json!({"recovered": true})),
        ],
        Duration::from_secs(20),
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingTimer).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    let before = persisted_trigger_rows(&pool, run.run_uid).await?;
    assert_eq!(
        before
            .iter()
            .filter(|(kind, _, _)| kind == "task_timer")
            .count(),
        1
    );

    fixture.recreate_valkey_after_loss().await?;
    fixture.recreate_restate_after_loss().await?;
    fixture.restart_orchestrator().await?;

    let recovered_pool = PgPool::connect(&fixture.postgres_url).await?;
    let after = persisted_trigger_rows(&recovered_pool, run.run_uid).await?;
    assert_eq!(
        after, before,
        "dependency recovery rewrote the immutable trigger"
    );
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"recovered": true})));
    let delivered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_trigger \
         WHERE run_uid = $1 AND trigger_kind = 'task_timer' AND state = 'delivered'",
    )
    .bind(run.run_uid)
    .fetch_one(&recovered_pool)
    .await?;
    assert_eq!(
        delivered, 1,
        "recovery delivered the immutable timer more than once"
    );
    let delivery = sqlx::query(
        "SELECT dispatch.state, dispatch.delivery_attempts \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid = trigger.trigger_uid \
         WHERE trigger.run_uid = $1 AND trigger.trigger_kind = 'task_timer' \
           AND dispatch.dispatch_kind = 'trigger_delivery'",
    )
    .bind(run.run_uid)
    .fetch_one(&recovered_pool)
    .await?;
    assert_eq!(delivery.try_get::<String, _>("state")?, "delivered");
    assert_eq!(delivery.try_get::<i32, _>("delivery_attempts")?, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for destructive empty-Restate recovery"]
async fn running_ambiguous_attempt_is_not_redriven_after_total_restate_loss_service_e2e()
-> Result<()> {
    // Pins: Restate loss never turns a Running possibly-committed effect back
    // into a dispatchable attempt. Its exact TaskAttempt delivery timestamp is
    // immutable, while the re-driven watchdog alone settles UnknownOutcome.
    let tool_name = "long_horizon_restate_loss_ambiguous";
    let fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Task 12 total-Restate-loss ambiguous effect".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["case"],
                "properties": {"case": {"type": "string"}}
            }),
            item_key_pointer: None,
            idempotent: false,
            outcomes: vec![FixtureCapabilityOutcome::ApplyThenDisconnect],
        }],
        vec![
            (
                "MOA_EXECUTION_ACTIVE_ATTEMPT_TIMEOUT_SECONDS".to_string(),
                "15".to_string(),
            ),
            // Staleness must stay strictly below the attempt timeout, so a scenario that
            // shortens the timeout has to shorten this with it or the orchestrator refuses
            // to boot.
            (
                "MOA_EXECUTION_ATTEMPT_HEARTBEAT_STALENESS_SECONDS".to_string(),
                "10".to_string(),
            ),
            (
                "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
                "1".to_string(),
            ),
        ],
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "restate-loss-running-ambiguous",
        vec![
            fixture_capability_node(
                "lost-running-attempt",
                tool_name,
                json!({"case": "possible-commit-before-loss"}),
            ),
            output_node(
                &["lost-running-attempt"],
                json!({"unexpected": "ambiguous effect replayed"}),
            ),
        ],
        Duration::from_secs(55),
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("Restate-loss fixture omitted capability controller")?;
    controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    let running = await_task_status(
        &test,
        &run,
        "lost-running-attempt",
        ExecutionTaskStatus::Running,
    )
    .await?;
    let accepted = sqlx::query(
        "SELECT dispatch_uid, delivered_at FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND task_id = $2 AND dispatch_kind = 'task_attempt' \
           AND attempt_generation = 1 AND state = 'delivered'",
    )
    .bind(run.run_uid)
    .bind(task_id(&running).as_uuid())
    .fetch_one(&pool)
    .await?;
    let dispatch_uid: Uuid = accepted.try_get("dispatch_uid")?;
    let delivered_at: DateTime<Utc> = accepted
        .try_get::<Option<DateTime<Utc>>, _>("delivered_at")?
        .context("accepted TaskAttempt omitted delivered_at")?;

    fixture.recreate_restate_after_loss().await?;
    fixture.hard_crash_and_restart_orchestrator().await?;
    // The fixture effect was committed before this gate. Release only after the
    // old handler is gone so the recovered watchdog owns ambiguity settlement.
    controller.release(1);

    let ambiguous = await_task_status(
        &test,
        &run,
        "lost-running-attempt",
        ExecutionTaskStatus::UnknownOutcome,
    )
    .await?;
    assert_eq!(ambiguous.attempt, 1);
    assert!(matches!(
        ambiguous.outcome.as_ref().map(|outcome| &outcome.result),
        Some(moa_artifacts::execution_plan::ExecutionTaskResult::UnknownOutcome { message })
            if message.contains("possible commit")
    ));
    let unchanged = sqlx::query(
        "SELECT state, delivered_at, delivery_attempts FROM moa.execution_dispatch_outbox \
         WHERE dispatch_uid = $1",
    )
    .bind(dispatch_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(unchanged.try_get::<String, _>("state")?, "delivered");
    assert_eq!(
        unchanged.try_get::<Option<DateTime<Utc>>, _>("delivered_at")?,
        Some(delivered_at),
        "reconciliation blindly re-drove a Running TaskAttempt"
    );
    assert_eq!(
        unchanged.try_get::<i32, _>("delivery_attempts")?,
        1,
        "Running TaskAttempt delivery was retried after Restate loss"
    );
    assert_eq!(
        controller.effect_count(),
        1,
        "Running ambiguous effect was logically sent more than once"
    );
    let watchdogs = sqlx::query(
        "SELECT trigger.state AS trigger_state, dispatch.state AS dispatch_state, \
                dispatch.delivery_attempts \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid = trigger.trigger_uid \
          AND dispatch.dispatch_kind = 'trigger_delivery' \
         WHERE trigger.run_uid = $1 AND trigger.task_id = $2 \
           AND trigger.trigger_kind = 'task_watchdog' \
           AND trigger.attempt_generation = 1",
    )
    .bind(run.run_uid)
    .bind(task_id(&running).as_uuid())
    .fetch_all(&pool)
    .await?;
    assert_eq!(watchdogs.len(), 1, "recovery created duplicate watchdogs");
    let watchdog = &watchdogs[0];
    assert_eq!(
        watchdog.try_get::<String, _>("trigger_state")?,
        "superseded"
    );
    assert_eq!(
        watchdog.try_get::<String, _>("dispatch_state")?,
        "cancelled"
    );
    assert_eq!(watchdog.try_get::<i32, _>("delivery_attempts")?, 1);
    Ok(())
}
