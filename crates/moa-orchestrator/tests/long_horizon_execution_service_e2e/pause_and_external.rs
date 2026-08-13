//! Pause/resume, parked wait settlement, and runtime-cache loss coverage.

use moa_artifacts::execution_plan::{ExecutionCitation, ExecutionTaskResult, InputAudience};
use moa_core::types::action_policy::ActionReviewStatus;
use moa_core::types::tools::{AsyncToolJobCallbackOutcome, AsyncToolJobTerminalOutcome};
use moa_execution::wire::{
    ExecutionConflictReason, ExecutionInputRequest, ExecutionMutationResponse,
    ExecutionSignalRequest,
};
use moa_orchestrator::services::{
    action_reviews::{
        ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
        ListActionReviewsRequest,
    },
    execution::{ExecutionRunControlRequest, ExecutionRunControlResponse},
};

use super::*;

async fn post_external_job_callback(
    fixture: &OrchestratorTestFixture,
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: &str,
    body: &Value,
) -> Result<reqwest::StatusCode> {
    let response = reqwest::Client::new()
        .post(fixture.external_job_callback_url(
            external_job_uid,
            job_generation,
            provider_event_id,
        )?)
        .bearer_auth(moa_test_support::FIXTURE_EXTERNAL_JOB_CALLBACK_TOKEN)
        .json(body)
        .send()
        .await?;
    Ok(response.status())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn true_external_job_reserves_before_start_parks_rearms_and_dedupes_callbacks_service_e2e()
-> Result<()> {
    // Pins: an async-capable production catalog tool reserves its stable job identity before the
    // provider start, binds the same idempotency key, releases attempt compute while waiting,
    // rearms sparse reconciliation from progress, and accepts each callback effect exactly once.
    let fixture = external_job_execution_fixture(Vec::new()).await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "true-external-job",
        vec![
            external_job_capability_node("external-job", json!({"value": "task12"})),
            output_node(&["external-job"], json!({"external": "complete"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    let controller = fixture
        .fixture_external_job()
        .context("external-job fixture omitted provider controller")?;
    let starts = controller.wait_for_starts(1, SCENARIO_TIMEOUT).await?;
    let start = &starts[0];
    assert_eq!(
        start.context.provider,
        moa_test_support::FIXTURE_EXTERNAL_JOB_PROVIDER
    );
    let reserved = sqlx::query(
        "SELECT job.state, job.idempotency_key, job.provider, job.provider_job_id, \
                capacity.state AS capacity_state, task.attempt_state, task.active_dispatch_uid \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.external_job_uid = job.external_job_uid \
          AND capacity.resource_dimension = 'external_jobs' \
         JOIN moa.execution_task AS task \
           ON task.run_uid = job.run_uid AND task.task_id = job.task_id \
         WHERE job.run_uid = $1 AND job.external_job_uid = $2",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(reserved.try_get::<String, _>("state")?, "unbound");
    assert_eq!(
        reserved.try_get::<String, _>("idempotency_key")?,
        start.context.idempotency_key
    );
    assert_eq!(reserved.try_get::<Option<String>, _>("provider")?, None);
    assert_eq!(
        reserved.try_get::<Option<String>, _>("provider_job_id")?,
        None
    );
    assert_eq!(reserved.try_get::<String, _>("capacity_state")?, "reserved");
    assert!(matches!(
        reserved.try_get::<String, _>("attempt_state")?.as_str(),
        "dispatching" | "running"
    ));
    assert!(
        reserved
            .try_get::<Option<Uuid>, _>("active_dispatch_uid")?
            .is_some()
    );

    controller.release_starts(1);
    let after_bind = controller.wait_for_after_bind(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(after_bind[0].context, start.context);
    controller.release_after_bind(1);
    await_run_status(&test, &run, ExecutionRunStatus::WaitingExternal).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "external-job",
        ExecutionTaskStatus::WaitingExternal,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    let bound = sqlx::query(
        "SELECT state, provider, provider_job_id, idempotency_key, job_generation, \
                progress_phase, next_reconcile_at, \
                (SELECT COUNT(*) FROM moa.execution_capacity_reservation \
                 WHERE external_job_uid = job.external_job_uid \
                   AND resource_dimension = 'external_jobs' AND state <> 'released') \
                   AS active_external_receipts \
         FROM moa.execution_external_job AS job \
         WHERE run_uid = $1 AND task_id = $2",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(bound.try_get::<String, _>("state")?, "running");
    assert_eq!(
        bound.try_get::<String, _>("provider")?,
        moa_test_support::FIXTURE_EXTERNAL_JOB_PROVIDER
    );
    assert_eq!(
        bound.try_get::<String, _>("provider_job_id")?,
        start.provider_job_id
    );
    assert_eq!(
        bound.try_get::<String, _>("idempotency_key")?,
        start.context.idempotency_key
    );
    assert_eq!(bound.try_get::<i64, _>("active_external_receipts")?, 1);
    let job_generation = u64::try_from(bound.try_get::<i64, _>("job_generation")?)?;

    let next_reconcile_at = moa_test_support::fixtures::pg_now() + TimeDelta::seconds(10);
    let progress_event_id = "task12-progress-1";
    let progress = controller.callback_body(
        start.provider_job_id.clone(),
        progress_event_id,
        AsyncToolJobCallbackOutcome::Progress {
            progress_phase: "halfway".to_string(),
            next_reconcile_at,
        },
    );
    assert_eq!(
        post_external_job_callback(
            &fixture,
            start.context.external_job_uid,
            job_generation,
            progress_event_id,
            &progress,
        )
        .await?,
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        post_external_job_callback(
            &fixture,
            start.context.external_job_uid,
            job_generation,
            progress_event_id,
            &progress,
        )
        .await?,
        reqwest::StatusCode::NO_CONTENT
    );
    let progressed = sqlx::query(
        "SELECT job.state, job.progress_phase, job.next_reconcile_at, \
                (SELECT COUNT(*) FROM moa.execution_external_job_callback_receipt AS receipt \
                 WHERE receipt.external_job_uid = job.external_job_uid \
                   AND receipt.provider_event_id = $2) AS receipt_count, \
                (SELECT COUNT(*) FROM moa.execution_trigger AS trigger \
                 WHERE trigger.payload->>'external_job_uid' = job.external_job_uid::TEXT \
                   AND (trigger.payload->>'job_generation')::BIGINT = job.job_generation \
                   AND trigger.trigger_kind = 'external_reconcile' \
                   AND trigger.state = 'pending' \
                   AND trigger.due_at = $3) AS exact_reconcile_triggers \
         FROM moa.execution_external_job AS job WHERE job.external_job_uid = $1",
    )
    .bind(start.context.external_job_uid)
    .bind(progress_event_id)
    .bind(next_reconcile_at)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        progressed.try_get::<String, _>("state")?,
        "waiting_reconcile"
    );
    assert_eq!(
        progressed.try_get::<String, _>("progress_phase")?,
        "halfway"
    );
    assert_eq!(
        progressed.try_get::<DateTime<Utc>, _>("next_reconcile_at")?,
        next_reconcile_at
    );
    assert_eq!(progressed.try_get::<i64, _>("receipt_count")?, 1);
    assert_eq!(progressed.try_get::<i64, _>("exact_reconcile_triggers")?, 1);
    await_run_status(&test, &run, ExecutionRunStatus::WaitingExternal).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let terminal_event_id = "task12-terminal-1";
    let terminal_body = controller.callback_body(
        start.provider_job_id.clone(),
        terminal_event_id,
        AsyncToolJobCallbackOutcome::Terminal {
            outcome: AsyncToolJobTerminalOutcome::Completed {
                output: json!({"provider": "done"}),
            },
        },
    );
    assert_eq!(
        post_external_job_callback(
            &fixture,
            start.context.external_job_uid,
            job_generation,
            terminal_event_id,
            &terminal_body,
        )
        .await?,
        reqwest::StatusCode::NO_CONTENT
    );
    let completed = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(completed.output, Some(json!({"external": "complete"})));
    assert_eq!(
        post_external_job_callback(
            &fixture,
            start.context.external_job_uid,
            job_generation,
            terminal_event_id,
            &terminal_body,
        )
        .await?,
        reqwest::StatusCode::NO_CONTENT
    );
    let terminal = sqlx::query(
        "SELECT job.state, job.output, job.completed_at, \
                (SELECT COUNT(*) FROM moa.execution_external_job_callback_receipt AS receipt \
                 WHERE receipt.external_job_uid = job.external_job_uid \
                   AND receipt.provider_event_id = $2) AS receipt_count, \
                (SELECT COUNT(*) FROM moa.execution_capacity_reservation AS capacity \
                 WHERE capacity.external_job_uid = job.external_job_uid \
                   AND capacity.resource_dimension = 'external_jobs' \
                   AND capacity.state = 'released') AS released_external_receipts \
         FROM moa.execution_external_job AS job WHERE job.external_job_uid = $1",
    )
    .bind(start.context.external_job_uid)
    .bind(terminal_event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal.try_get::<String, _>("state")?, "completed");
    assert_eq!(
        terminal.try_get::<Value, _>("output")?,
        json!({"provider": "done"})
    );
    assert!(
        terminal
            .try_get::<Option<DateTime<Utc>>, _>("completed_at")?
            .is_some()
    );
    assert_eq!(terminal.try_get::<i64, _>("receipt_count")?, 1);
    assert_eq!(terminal.try_get::<i64, _>("released_external_receipts")?, 1);
    assert_eq!(controller.starts().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn terminal_callback_before_attempt_release_defers_then_settles_once_service_e2e()
-> Result<()> {
    // Pins: a provider terminal callback can win after durable bind but before TaskAttempt has
    // released its capacity. Persistence records DeferredRelease without waking the controller;
    // the exact post-bind attempt then releases capacity and consumes that terminal result once.
    let fixture = external_job_execution_fixture(Vec::new()).await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "external-terminal-before-release",
        vec![
            external_job_capability_node("external-job", json!({"value": "early-terminal"})),
            output_node(&["external-job"], json!({"deferred_release": "complete"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    let controller = fixture
        .fixture_external_job()
        .context("external-job fixture omitted provider controller")?;
    let starts = controller.wait_for_starts(1, SCENARIO_TIMEOUT).await?;
    let start = &starts[0];
    controller.release_starts(1);
    let after_bind = controller.wait_for_after_bind(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(after_bind[0].context, start.context);
    let bound_owner = sqlx::query(
        "SELECT job.job_generation, job.state AS job_state, task.status AS task_status, \
                task.attempt_state, task.active_dispatch_uid, \
                capacity.state AS task_capacity_state \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_task AS task \
           ON task.run_uid = job.run_uid AND task.task_id = job.task_id \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.run_uid = task.run_uid AND capacity.task_id = task.task_id \
          AND capacity.attempt_generation = task.attempt_generation \
          AND capacity.resource_dimension = 'active_tasks' \
         WHERE job.run_uid = $1 AND job.external_job_uid = $2",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid)
    .fetch_one(&pool)
    .await?;
    let job_generation = u64::try_from(bound_owner.try_get::<i64, _>("job_generation")?)?;
    assert_eq!(bound_owner.try_get::<String, _>("job_state")?, "running");
    assert_eq!(bound_owner.try_get::<String, _>("task_status")?, "running");
    assert_eq!(
        bound_owner.try_get::<String, _>("attempt_state")?,
        "running"
    );
    assert_eq!(
        bound_owner.try_get::<String, _>("task_capacity_state")?,
        "reserved"
    );
    assert!(
        bound_owner
            .try_get::<Option<Uuid>, _>("active_dispatch_uid")?
            .is_some()
    );

    let event_id = "task12-terminal-before-release";
    let body = controller.callback_body(
        start.provider_job_id.clone(),
        event_id,
        AsyncToolJobCallbackOutcome::Terminal {
            outcome: AsyncToolJobTerminalOutcome::Completed {
                output: json!({"provider": "early"}),
            },
        },
    );
    assert_eq!(
        post_external_job_callback(
            &fixture,
            start.context.external_job_uid,
            job_generation,
            event_id,
            &body,
        )
        .await?,
        reqwest::StatusCode::NO_CONTENT
    );
    let deferred = sqlx::query(
        "SELECT job.state AS job_state, task.status AS task_status, task.attempt_state, \
                task.active_dispatch_uid, task.external_job_uid, \
                task_capacity.state AS task_capacity_state, \
                external_capacity.state AS external_capacity_state, \
                (SELECT COUNT(*) FROM moa.execution_dispatch_outbox AS activation \
                 WHERE activation.run_uid = job.run_uid \
                   AND activation.dispatch_kind = 'run_activation' \
                   AND activation.payload ->> 'source' = 'external_job_callback' \
                   AND activation.payload ->> 'external_job_uid' = $2) AS callback_activations \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_task AS task \
           ON task.run_uid = job.run_uid AND task.task_id = job.task_id \
         JOIN moa.execution_capacity_reservation AS task_capacity \
           ON task_capacity.run_uid = task.run_uid AND task_capacity.task_id = task.task_id \
          AND task_capacity.attempt_generation = task.attempt_generation \
          AND task_capacity.resource_dimension = 'active_tasks' \
         JOIN moa.execution_capacity_reservation AS external_capacity \
           ON external_capacity.external_job_uid = job.external_job_uid \
          AND external_capacity.resource_dimension = 'external_jobs' \
         WHERE job.run_uid = $1 AND job.external_job_uid = $3",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid.to_string())
    .bind(start.context.external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(deferred.try_get::<String, _>("job_state")?, "completed");
    assert_eq!(deferred.try_get::<String, _>("task_status")?, "running");
    assert_eq!(deferred.try_get::<String, _>("attempt_state")?, "running");
    assert_eq!(
        deferred.try_get::<Option<Uuid>, _>("external_job_uid")?,
        None,
        "callback must not forge the pre-release checkpoint"
    );
    assert!(
        deferred
            .try_get::<Option<Uuid>, _>("active_dispatch_uid")?
            .is_some()
    );
    assert_eq!(
        deferred.try_get::<String, _>("task_capacity_state")?,
        "reserved"
    );
    assert_eq!(
        deferred.try_get::<String, _>("external_capacity_state")?,
        "released"
    );
    assert_eq!(deferred.try_get::<i64, _>("callback_activations")?, 0);

    controller.release_after_bind(1);
    let completed = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(
        completed.output,
        Some(json!({"deferred_release": "complete"}))
    );
    let settled = sqlx::query(
        "SELECT task.status, task.attempt_state, task.active_dispatch_uid, \
                task.external_job_uid, capacity.state AS capacity_state, \
                (SELECT COUNT(*) FROM moa.execution_external_job_callback_receipt \
                 WHERE external_job_uid = $2 AND provider_event_id = $3) AS receipt_count \
         FROM moa.execution_task AS task \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.run_uid = task.run_uid AND capacity.task_id = task.task_id \
          AND capacity.attempt_generation = task.attempt_generation \
          AND capacity.resource_dimension = 'active_tasks' \
         WHERE task.run_uid = $1 AND task.node_id = 'external-job'",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid)
    .bind(event_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(settled.try_get::<String, _>("status")?, "completed");
    assert_eq!(settled.try_get::<String, _>("attempt_state")?, "terminal");
    assert_eq!(
        settled.try_get::<Option<Uuid>, _>("active_dispatch_uid")?,
        None
    );
    assert_eq!(
        settled.try_get::<Option<Uuid>, _>("external_job_uid")?,
        Some(start.context.external_job_uid)
    );
    assert_eq!(settled.try_get::<String, _>("capacity_state")?, "released");
    assert_eq!(settled.try_get::<i64, _>("receipt_count")?, 1);
    assert_eq!(controller.starts().len(), 1);
    assert_eq!(controller.after_bind().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn paused_external_reconcile_settles_storage_then_resume_activates_once_service_e2e()
-> Result<()> {
    // Pins: provider time continues while a run is Paused. A due sparse reconciliation persists
    // the terminal job/task outcome without controller compute, and resume emits one activation
    // that observes the already-settled dependency instead of reissuing provider work.
    let fixture = external_job_execution_fixture(Vec::new()).await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "paused-external-reconcile",
        vec![
            external_job_capability_node("external-job", json!({"value": "pause"})),
            output_node(&["external-job"], json!({"external_pause": "complete"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    let controller = fixture
        .fixture_external_job()
        .context("external-job fixture omitted provider controller")?;
    let starts = controller.wait_for_starts(1, SCENARIO_TIMEOUT).await?;
    let start = &starts[0];
    controller.release_starts(1);
    let after_bind = controller.wait_for_after_bind(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(after_bind[0].context, start.context);
    controller.release_after_bind(1);
    await_run_status(&test, &run, ExecutionRunStatus::WaitingExternal).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "external-job",
        ExecutionTaskStatus::WaitingExternal,
    )
    .await?;
    let before_pause = sqlx::query(
        "SELECT run.controller_generation, job.job_generation, job.next_reconcile_at, \
                trigger.trigger_uid, trigger.due_at, trigger.state AS trigger_state \
         FROM moa.execution_run AS run \
         JOIN moa.execution_external_job AS job ON job.run_uid = run.run_uid \
         JOIN moa.execution_trigger AS trigger \
           ON trigger.payload->>'external_job_uid' = job.external_job_uid::TEXT \
          AND (trigger.payload->>'job_generation')::BIGINT = job.job_generation \
          AND trigger.trigger_kind = 'external_reconcile' \
         WHERE run.run_uid = $1 AND job.task_id = $2",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .fetch_one(&pool)
    .await?;
    let initial_generation =
        u64::try_from(before_pause.try_get::<i64, _>("controller_generation")?)?;
    let job_generation = u64::try_from(before_pause.try_get::<i64, _>("job_generation")?)?;
    let reconcile_trigger_uid: Uuid = before_pause.try_get("trigger_uid")?;
    let due_at: DateTime<Utc> = before_pause.try_get("due_at")?;
    assert_eq!(
        before_pause.try_get::<DateTime<Utc>, _>("next_reconcile_at")?,
        due_at
    );
    assert_eq!(
        before_pause.try_get::<String, _>("trigger_state")?,
        "pending"
    );

    let pause_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: initial_generation,
    };
    let pause: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/pause", &pause_request)
        .await?;
    let paused_generation = match pause {
        ExecutionRunControlResponse::Applied {
            run: summary,
            controller_generation,
            ..
        } => {
            assert_eq!(summary.status, ExecutionRunStatus::Paused);
            assert_eq!(controller_generation, initial_generation + 1);
            controller_generation
        }
        other => bail!("external waiting run did not pause: {other:?}"),
    };
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    let paused_external_receipts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND external_job_uid = $2 \
           AND resource_dimension = 'external_jobs' AND state <> 'released'",
    )
    .bind(run.run_uid)
    .bind(start.context.external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(paused_external_receipts, 1);
    controller.queue_reconcile_outcomes([AsyncToolJobCallbackOutcome::Terminal {
        outcome: AsyncToolJobTerminalOutcome::Completed {
            output: json!({"provider": "reconciled"}),
        },
    }]);
    let reconciliations = controller
        .wait_for_reconciliations(1, SCENARIO_TIMEOUT)
        .await?;
    assert_eq!(reconciliations.len(), 1);
    assert_eq!(
        reconciliations[0].request.external_job_uid,
        start.context.external_job_uid
    );
    assert_eq!(reconciliations[0].request.job_generation, job_generation);
    assert_eq!(
        reconciliations[0].request.trigger_uid,
        reconcile_trigger_uid
    );
    assert_eq!(
        reconciliations[0].request.idempotency_key,
        start.context.idempotency_key
    );

    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    let settled = loop {
        let row = sqlx::query(
            "SELECT run.status AS run_status, run.activation_state, run.controller_generation, \
                    job.state AS job_state, job.output, task.status AS task_status, \
                    trigger.state AS trigger_state, dispatch.state AS dispatch_state, \
                    (SELECT COUNT(*) FROM moa.execution_capacity_reservation AS capacity \
                     WHERE capacity.external_job_uid = job.external_job_uid \
                       AND capacity.resource_dimension = 'external_jobs' \
                       AND capacity.state = 'released') AS released_external_receipts, \
                    (SELECT COUNT(*) FROM moa.execution_dispatch_outbox AS activation \
                     WHERE activation.run_uid = run.run_uid \
                       AND activation.dispatch_kind = 'run_activation' \
                       AND activation.controller_generation = $3) AS paused_activations \
             FROM moa.execution_run AS run \
             JOIN moa.execution_external_job AS job ON job.run_uid = run.run_uid \
             JOIN moa.execution_task AS task \
               ON task.run_uid = job.run_uid AND task.task_id = job.task_id \
             JOIN moa.execution_trigger AS trigger ON trigger.trigger_uid = $2 \
             JOIN moa.execution_dispatch_outbox AS dispatch \
               ON dispatch.trigger_uid = trigger.trigger_uid \
              AND dispatch.dispatch_kind = 'trigger_delivery' \
             WHERE run.run_uid = $1",
        )
        .bind(run.run_uid)
        .bind(reconcile_trigger_uid)
        .bind(i64::try_from(paused_generation)?)
        .fetch_one(&pool)
        .await?;
        if row.try_get::<String, _>("job_state")? == "completed"
            && row.try_get::<String, _>("task_status")? == "completed"
            && row.try_get::<String, _>("trigger_state")? == "superseded"
            && row.try_get::<String, _>("dispatch_state")? == "cancelled"
        {
            break row;
        }
        if Instant::now() >= deadline {
            bail!(
                "paused external reconciliation did not settle: job={}, task={}, trigger={}",
                row.try_get::<String, _>("job_state")?,
                row.try_get::<String, _>("task_status")?,
                row.try_get::<String, _>("trigger_state")?,
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    assert_eq!(settled.try_get::<String, _>("run_status")?, "paused");
    assert_eq!(settled.try_get::<String, _>("activation_state")?, "paused");
    assert_eq!(
        u64::try_from(settled.try_get::<i64, _>("controller_generation")?)?,
        paused_generation
    );
    assert_eq!(settled.try_get::<String, _>("trigger_state")?, "superseded");
    assert_eq!(settled.try_get::<String, _>("dispatch_state")?, "cancelled");
    assert_eq!(settled.try_get::<i64, _>("released_external_receipts")?, 1);
    assert_eq!(
        settled.try_get::<Value, _>("output")?,
        json!({"provider": "reconciled"})
    );
    assert_eq!(settled.try_get::<i64, _>("paused_activations")?, 0);
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let resume_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: paused_generation,
    };
    let resume: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/resume", &resume_request)
        .await?;
    let (resumed_generation, resumed_wake_epoch) = match resume {
        ExecutionRunControlResponse::Applied {
            controller_generation,
            wake_epoch,
            ..
        } => {
            assert_eq!(controller_generation, paused_generation + 1);
            (controller_generation, wake_epoch)
        }
        other => bail!("settled external run did not resume: {other:?}"),
    };
    let completed = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(
        completed.output,
        Some(json!({"external_pause": "complete"}))
    );
    let resumed_activations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'run_activation' \
           AND controller_generation = $2 AND wake_epoch = $3",
    )
    .bind(run.run_uid)
    .bind(i64::try_from(resumed_generation)?)
    .bind(i64::try_from(resumed_wake_epoch)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(resumed_activations, 1);
    assert_eq!(controller.starts().len(), 1);
    assert_eq!(controller.reconciliations().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn agent_action_review_parks_then_resumes_persisted_continuation_service_e2e() -> Result<()> {
    // Pins: an Agent's governed effect is checkpointed before WaitingReview,
    // owns no active capacity while parked, and a tenant-admin decision resumes
    // the persisted invocation in a new bounded slice instead of a live promise.
    let tool_name = "long_horizon_agent_review_probe";
    let registered_tool_name = moa_hands::mcp_tool_reference("fixture-capability", tool_name);
    let completed = serde_json::to_string(&ExecutionTaskResult::Completed {
        output: json!({"review": "continued"}),
        citations: Vec::<ExecutionCitation>::new(),
    })?;
    let fixture = execution_fixture_with_script_and_tools(
        json!({
            "default": {
                "completion": {
                    "content": "unexpected agent review continuation path",
                    "tool_calls": []
                }
            },
            "keyed": [
                {
                    "match": "reviewed_effect",
                    "completion": {"content": completed, "tool_calls": []}
                },
                {
                    "match": "agent-review-operation",
                    "completion": {
                        "content": "Requesting the reviewed effect.",
                        "tool_calls": [{
                            "name": registered_tool_name,
                            "id": "agent-review-effect",
                            "input": {"case": "agent-review"}
                        }]
                    }
                }
            ]
        }),
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 Agent review continuation".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["case"],
                "properties": {"case": {"type": "string"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::Success {
                output: json!({"reviewed_effect": "applied"}),
            }],
        }],
        Vec::new(),
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan_with_capability_policy(
        &test,
        "agent-action-review",
        vec![
            node(
                "reviewing-agent",
                &[],
                ExecutionOperation::Agent {
                    instructions: "agent-review-operation".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: vec![CapabilityReference {
                        name: registered_tool_name.clone(),
                        version: FIXTURE_CAPABILITY_VERSION.to_string(),
                    }],
                    max_turns: 3,
                },
                json!({"type": "object"}),
            ),
            output_node(&["reviewing-agent"], json!({"review": "complete"})),
        ],
        Duration::from_secs(30),
        Some(ActionPolicyEffect::AdminReview),
    )
    .await?;
    let waiting = await_task_status(
        &test,
        &run,
        "reviewing-agent",
        ExecutionTaskStatus::WaitingReview,
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingReview).await?;
    assert_eq!(waiting.attempt, 1);
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let review = await_execution_action_review(&test, run.tenant_id, task_id(&waiting)).await?;
    let origin = review
        .envelope
        .owner
        .execution_origin()
        .context("Agent action review omitted execution origin")?;
    assert_eq!(origin.task_uid, task_id(&waiting).as_uuid());
    assert_eq!(origin.generation, waiting.generation);
    let checkpoint = sqlx::query(
        "SELECT checkpoint_kind, task_generation, attempt_generation, payload, \
                workspace_release_receipt, \
                (SELECT COUNT(*) FROM moa.execution_capacity_reservation AS capacity \
                 WHERE capacity.run_uid = checkpoint.run_uid \
                   AND capacity.task_id = checkpoint.task_id \
                   AND capacity.attempt_generation = checkpoint.attempt_generation \
                   AND capacity.resource_dimension = 'active_tasks' \
                   AND capacity.state = 'released') AS released_capacity, \
                (SELECT COUNT(*) FROM moa.execution_dispatch_outbox AS activation \
                 WHERE activation.run_uid = checkpoint.run_uid \
                   AND activation.dispatch_kind = 'run_activation' \
                   AND activation.payload->>'source' = 'task_attempt_review_park' \
                   AND activation.payload->>'task_id' = checkpoint.task_id::TEXT \
                   AND (activation.payload->>'attempt_generation')::BIGINT = \
                       checkpoint.attempt_generation) AS controller_activations \
         FROM moa.execution_task_checkpoint AS checkpoint \
         WHERE checkpoint.run_uid = $1 AND checkpoint.task_id = $2 \
           AND checkpoint.superseded_at IS NULL",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        checkpoint.try_get::<String, _>("checkpoint_kind")?,
        "agent_continuation"
    );
    assert_eq!(
        u64::try_from(checkpoint.try_get::<i64, _>("task_generation")?)?,
        waiting.generation
    );
    assert_eq!(checkpoint.try_get::<i64, _>("attempt_generation")?, 2);
    assert_eq!(checkpoint.try_get::<i64, _>("released_capacity")?, 1);
    assert_eq!(checkpoint.try_get::<i64, _>("controller_activations")?, 1);
    let release_receipt: Value = checkpoint
        .try_get::<Option<Value>, _>("workspace_release_receipt")?
        .context("the review checkpoint omitted its exact hand-release proof")?;
    assert_eq!(release_receipt.get("workspace_id"), Some(&Value::Null));
    assert_eq!(
        release_receipt.get("hand_provisioning_operation_id"),
        Some(&Value::Null)
    );
    assert_eq!(
        release_receipt.get("hand_lease_generation"),
        Some(&Value::Null),
        "a non-sandbox review must persist verified absence, not invent a hand identity"
    );
    let payload: Value = checkpoint.try_get("payload")?;
    assert_eq!(
        payload.pointer("/state/kind").and_then(Value::as_str),
        Some("agent")
    );
    let review_id = review.id.to_string();
    assert_eq!(
        payload
            .pointer("/state/pending_review/review_uid")
            .and_then(Value::as_str),
        Some(review_id.as_str())
    );
    assert_eq!(
        payload
            .pointer("/state/pending_review/invocation/id")
            .and_then(Value::as_str),
        Some("agent-review-effect")
    );
    assert_eq!(payload.get("review_resolution"), Some(&Value::Null));
    let controller = fixture
        .fixture_capability()
        .context("Agent review fixture omitted capability controller")?;
    assert!(
        controller.calls().is_empty(),
        "reviewed effect ran before approval"
    );

    let client = fixture.client.clone();
    let tenant_id = run.tenant_id;
    let action_review_uid = review.id;
    let decision = tokio::spawn(async move {
        client
            .post_void(
                "/ActionReviews/decide",
                &DecideActionReviewRequest {
                    tenant_id,
                    review_id: action_review_uid,
                    decision: ActionReviewDecisionKind::Cleared,
                    reason: None,
                },
            )
            .await
    });
    let calls = controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, tool_name);
    assert_eq!(calls[0].input, json!({"case": "agent-review"}));
    controller.release(1);
    tokio::time::timeout(SCENARIO_TIMEOUT, decision)
        .await
        .context("Agent action-review decision did not durably settle")???;

    let (attempt_count, last_error) = await_execution_review_delivery(&pool, review.id).await?;
    assert_eq!(
        attempt_count, 1,
        "the admin decision must dispatch its exact execution resolution once; last_error={last_error:?}"
    );
    assert!(
        last_error.is_none(),
        "the exact execution resolution failed before delivery: {last_error:?}"
    );
    let completed_task = await_task_status(
        &test,
        &run,
        "reviewing-agent",
        ExecutionTaskStatus::Completed,
    )
    .await?;
    assert_eq!(completed_task.attempt, 1);
    assert_eq!(completed_task.generation, waiting.generation);
    assert_eq!(controller.calls().len(), 1);
    let redispatch = sqlx::query(
        "SELECT COUNT(*) AS dispatch_count, COUNT(DISTINCT dispatch_uid) AS distinct_dispatches, \
                MIN(attempt_generation) AS first_generation, \
                MAX(attempt_generation) AS last_generation \
         FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND task_id = $2 AND dispatch_kind = 'task_attempt'",
    )
    .bind(run.run_uid)
    .bind(task_id(&completed_task).as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(redispatch.try_get::<i64, _>("dispatch_count")?, 3);
    assert_eq!(redispatch.try_get::<i64, _>("distinct_dispatches")?, 3);
    assert_eq!(
        redispatch.try_get::<Option<i64>, _>("first_generation")?,
        Some(1)
    );
    assert_eq!(
        redispatch.try_get::<Option<i64>, _>("last_generation")?,
        Some(3)
    );
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"review": "complete"})));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey/sandbox-workspace fixture"]
async fn sandbox_hand_releases_during_signal_wait_and_reacquires_after_resume_service_e2e()
-> Result<()> {
    // Pins: a real sandbox-required execution capability owns one ActiveHands
    // receipt while running, releases it before a storage-only signal wait,
    // and a downstream sandbox task acquires and releases its own receipt.
    let fixture = sandbox_execution_fixture().await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "sandbox-hand-park-resume",
        vec![
            hand_capability_node(
                "sandbox-before-wait",
                &[],
                "bash",
                json!({
                    "cmd": "sleep 3",
                    "timeout_secs": 10
                }),
            ),
            node(
                "sandbox-signal-wait",
                &["sandbox-before-wait"],
                ExecutionOperation::WaitSignal {
                    signal_name: "resume-sandbox".to_string(),
                    wait_policy: ExecutionWaitPolicy {
                        expiry: after_logical_days(7),
                        on_expiry: ExecutionWaitExpiryAction::FailTask,
                    },
                },
                json!({"type": "object"}),
            ),
            hand_capability_node(
                "sandbox-after-wait",
                &["sandbox-signal-wait"],
                "bash",
                json!({
                    "cmd": "sleep 3",
                    "timeout_secs": 10
                }),
            ),
            output_node(
                &["sandbox-after-wait"],
                json!({"sandbox": "released-and-reacquired"}),
            ),
        ],
        Duration::from_secs(35),
    )
    .await?;

    let first_task = await_task_status(
        &test,
        &run,
        "sandbox-before-wait",
        ExecutionTaskStatus::Running,
    )
    .await?;
    let first_workspace =
        await_active_execution_task_hand(&pool, &run, task_id(&first_task)).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "sandbox-signal-wait",
        ExecutionTaskStatus::WaitingSignal,
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingSignal).await?;
    assert_released_execution_task_hand(&pool, &run, task_id(&first_task), first_workspace).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let signal: ExecutionMutationResponse = test
        .client()
        .post_call(
            "/Execution/deliver_signal",
            &ExecutionSignalRequest {
                tenant_id: run.tenant_id,
                contact_id: None,
                run_uid: run.run_uid,
                task_id: task_id(&waiting),
                expected_generation: waiting.generation,
                signal_name: "resume-sandbox".to_string(),
                payload: json!({"resume": true}),
            },
        )
        .await?;
    assert!(matches!(signal, ExecutionMutationResponse::Applied { .. }));
    let second_task = await_task_status(
        &test,
        &run,
        "sandbox-after-wait",
        ExecutionTaskStatus::Running,
    )
    .await?;
    let second_workspace =
        await_active_execution_task_hand(&pool, &run, task_id(&second_task)).await?;
    assert_ne!(
        second_workspace, first_workspace,
        "execution-task workspace ownership must remain scoped to the exact logical task"
    );
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(
        terminal.output,
        Some(json!({"sandbox": "released-and-reacquired"}))
    );
    assert_released_execution_task_hand(&pool, &run, task_id(&second_task), second_workspace)
        .await?;
    fixture.cleanup_sandbox_workspace_namespace().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn paused_timer_settles_without_activation_then_resume_advances_once_service_e2e()
-> Result<()> {
    // Pins: wall time continues while a storage-only timer run is Paused; the
    // due trigger and exact task settle durably without controller activation,
    // then one generation-fenced resume activation advances the settled graph.
    let fixture = execution_fixture(vec![(
        "MOA_EXECUTION_TRIGGER_RECONCILIATION_CADENCE_SECONDS".to_string(),
        "1".to_string(),
    )])
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "pause-due-timer",
        vec![
            node(
                "paused-timer",
                &[],
                ExecutionOperation::WaitUntil {
                    wake: after_logical_days(5),
                    result: json!({"timer": "elapsed"}),
                },
                json!({"type": "object"}),
            ),
            output_node(&["paused-timer"], json!({"pause_timer": "complete"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingTimer).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "paused-timer",
        ExecutionTaskStatus::WaitingTimer,
    )
    .await?;
    let timer_row = sqlx::query(
        "SELECT trigger.trigger_uid, trigger.due_at, run.controller_generation, run.wake_epoch \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_run AS run ON run.run_uid = trigger.run_uid \
         WHERE trigger.run_uid = $1 AND trigger.task_id = $2 \
           AND trigger.trigger_kind = 'task_timer'",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .fetch_one(&pool)
    .await?;
    let trigger_uid: Uuid = timer_row.try_get("trigger_uid")?;
    let due_at: DateTime<Utc> = timer_row.try_get("due_at")?;
    let initial_generation = u64::try_from(timer_row.try_get::<i64, _>("controller_generation")?)?;
    let initial_wake_epoch = u64::try_from(timer_row.try_get::<i64, _>("wake_epoch")?)?;
    assert!(
        due_at > Utc::now() + TimeDelta::seconds(2),
        "setup failed to pause the timer safely before its persisted due time"
    );

    let pause_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: initial_generation,
    };
    let pause: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/pause", &pause_request)
        .await?;
    let (paused_generation, paused_wake_epoch) = match pause {
        ExecutionRunControlResponse::Applied {
            run: summary,
            controller_generation,
            wake_epoch,
        } => {
            assert_eq!(summary.status, ExecutionRunStatus::Paused);
            assert_eq!(controller_generation, initial_generation + 1);
            assert!(wake_epoch >= initial_wake_epoch);
            (controller_generation, wake_epoch)
        }
        other => bail!("waiting-timer pause was not applied directly: {other:?}"),
    };
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    let settled = loop {
        let row = sqlx::query(
            "SELECT trigger.state AS trigger_state, trigger.delivered_at, \
                    trigger.controller_generation AS trigger_controller_generation, \
                    dispatch.state AS dispatch_state, dispatch.delivered_at AS dispatch_delivered_at, \
                    task.status AS task_status, task.outcome_audit, task.generation_history, \
                    run.status AS run_status, run.activation_state, \
                    run.controller_generation, run.wake_epoch, \
                    (SELECT capacity.state FROM moa.execution_capacity_reservation AS capacity \
                     WHERE capacity.trigger_uid = trigger.trigger_uid \
                       AND capacity.resource_dimension = 'scheduled_triggers') \
                       AS scheduled_trigger_capacity_state, \
                    (SELECT COUNT(*) FROM moa.execution_dispatch_outbox AS activation \
                     WHERE activation.run_uid = run.run_uid \
                       AND activation.dispatch_kind = 'run_activation' \
                       AND activation.controller_generation = $3) AS paused_activations \
             FROM moa.execution_trigger AS trigger \
             JOIN moa.execution_dispatch_outbox AS dispatch \
               ON dispatch.trigger_uid = trigger.trigger_uid \
              AND dispatch.dispatch_kind = 'trigger_delivery' \
             JOIN moa.execution_task AS task \
               ON task.run_uid = trigger.run_uid AND task.task_id = trigger.task_id \
             JOIN moa.execution_run AS run ON run.run_uid = trigger.run_uid \
             WHERE trigger.run_uid = $1 AND trigger.trigger_uid = $2",
        )
        .bind(run.run_uid)
        .bind(trigger_uid)
        .bind(i64::try_from(paused_generation)?)
        .fetch_one(&pool)
        .await?;
        if row.try_get::<String, _>("trigger_state")? == "delivered"
            && row.try_get::<String, _>("task_status")? == "completed"
        {
            break row;
        }
        if Instant::now() >= deadline {
            bail!(
                "paused timer {trigger_uid} did not settle after persisted due_at {due_at}; \
                 trigger={}, task={}",
                row.try_get::<String, _>("trigger_state")?,
                row.try_get::<String, _>("task_status")?,
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    assert_eq!(settled.try_get::<String, _>("dispatch_state")?, "delivered");
    let trigger_delivered_at = settled
        .try_get::<Option<DateTime<Utc>>, _>("delivered_at")?
        .context("delivered timer omitted delivered_at")?;
    let dispatch_delivered_at = settled
        .try_get::<Option<DateTime<Utc>>, _>("dispatch_delivered_at")?
        .context("delivered timer outbox omitted delivered_at")?;
    assert!(trigger_delivered_at >= due_at);
    assert!(dispatch_delivered_at >= due_at);
    assert_eq!(
        u64::try_from(settled.try_get::<i64, _>("trigger_controller_generation")?)?,
        initial_generation,
        "pause must not rearm the immutable timer under the new controller generation"
    );
    assert_eq!(
        settled.try_get::<String, _>("scheduled_trigger_capacity_state")?,
        "released"
    );
    assert_eq!(settled.try_get::<String, _>("run_status")?, "paused");
    assert_eq!(settled.try_get::<String, _>("activation_state")?, "paused");
    assert_eq!(
        u64::try_from(settled.try_get::<i64, _>("controller_generation")?)?,
        paused_generation
    );
    assert!(
        u64::try_from(settled.try_get::<i64, _>("wake_epoch")?)? >= paused_wake_epoch,
        "paused settlement must not move the run wake epoch backwards"
    );
    assert_eq!(settled.try_get::<i64, _>("paused_activations")?, 0);
    let outcome_audit: Value = settled.try_get("outcome_audit")?;
    assert_eq!(
        outcome_audit
            .as_array()
            .context("timer outcome audit was not an array")?
            .iter()
            .filter(|entry| entry.get("accepted").and_then(Value::as_bool) == Some(true))
            .count(),
        1,
        "the due timer outcome must be accepted exactly once while paused"
    );
    let generation_history: Value = settled.try_get("generation_history")?;
    assert_eq!(
        generation_history
            .as_array()
            .context("timer generation history was not an array")?
            .iter()
            .filter(|entry| {
                entry.get("kind").and_then(Value::as_str) == Some("storage_wait_settlement")
            })
            .count(),
        1,
        "the due timer must record exactly one storage-wait settlement"
    );
    await_run_status(&test, &run, ExecutionRunStatus::Paused).await?;
    await_task_status(&test, &run, "paused-timer", ExecutionTaskStatus::Completed).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let resume_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: paused_generation,
    };
    let resume: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/resume", &resume_request)
        .await?;
    let (resumed_generation, resumed_wake_epoch) = match resume {
        ExecutionRunControlResponse::Applied {
            run: summary,
            controller_generation,
            wake_epoch,
        } => {
            assert_eq!(summary.status, ExecutionRunStatus::Queued);
            assert_eq!(controller_generation, paused_generation + 1);
            (controller_generation, wake_epoch)
        }
        other => bail!("paused settled timer did not resume: {other:?}"),
    };
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"pause_timer": "complete"})));
    let resumed_activation_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'run_activation' \
           AND controller_generation = $2 AND wake_epoch = $3",
    )
    .bind(run.run_uid)
    .bind(i64::try_from(resumed_generation)?)
    .bind(i64::try_from(resumed_wake_epoch)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        resumed_activation_count, 1,
        "resume must enqueue exactly one activation for its returned generation and wake epoch"
    );
    let final_counts = sqlx::query(
        "SELECT \
           (SELECT COUNT(*) FROM moa.execution_trigger \
            WHERE run_uid = $1 AND task_id = $2 AND trigger_kind = 'task_timer' \
              AND state = 'delivered') AS delivered_timers, \
           (SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
            WHERE run_uid = $1 AND dispatch_kind = 'run_activation' \
              AND controller_generation = $3 AND wake_epoch = $5) AS resumed_activations, \
           (SELECT COUNT(*) FROM moa.execution_capacity_reservation \
            WHERE run_uid = $1 AND resource_dimension = 'parked_runs' \
              AND controller_generation = $4 AND state = 'released') \
              AS released_paused_receipts",
    )
    .bind(run.run_uid)
    .bind(task_id(&waiting).as_uuid())
    .bind(i64::try_from(resumed_generation)?)
    .bind(i64::try_from(paused_generation)?)
    .bind(i64::try_from(resumed_wake_epoch)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(final_counts.try_get::<i64, _>("delivered_timers")?, 1);
    assert_eq!(final_counts.try_get::<i64, _>("resumed_activations")?, 1);
    assert_eq!(
        final_counts.try_get::<i64, _>("released_paused_receipts")?,
        1,
        "resume must release the exact paused-generation ParkedRuns receipt"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn active_attempt_pause_drains_then_generation_fenced_resume_completes_service_e2e()
-> Result<()> {
    // Pins: pause fences a running bounded attempt, drains all compute before
    // Paused, replays the exact control request, and resume advances one generation.
    let tool_name = "long_horizon_pause_probe";
    let fixture = execution_fixture_with_tools(
        vec![FixtureCapabilityTool {
            name: tool_name.to_string(),
            description: "Deterministic Task 12 pause barrier".to_string(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["case"],
                "properties": {"case": {"type": "string"}}
            }),
            item_key_pointer: None,
            idempotent: true,
            outcomes: vec![FixtureCapabilityOutcome::Success {
                output: json!({"result": "resumed"}),
            }],
        }],
        Vec::new(),
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "pause-resume",
        vec![
            fixture_capability_node("pausable-agent", tool_name, json!({"case": "pause-resume"})),
            output_node(&["pausable-agent"], json!({"pause": "resumed"})),
        ],
        Duration::from_secs(30),
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("pause fixture omitted capability controller")?;
    let first = controller.wait_for_calls(1, SCENARIO_TIMEOUT).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].capability, tool_name);
    let running =
        await_task_status(&test, &run, "pausable-agent", ExecutionTaskStatus::Running).await?;
    let active_before_pause = sqlx::query(
        "SELECT generation, attempt_generation, active_dispatch_uid \
         FROM moa.execution_task WHERE run_uid = $1 AND task_id = $2",
    )
    .bind(run.run_uid)
    .bind(task_id(&running).as_uuid())
    .fetch_one(&pool)
    .await?;
    let task_generation = u64::try_from(active_before_pause.try_get::<i64, _>("generation")?)?;
    let attempt_generation =
        u64::try_from(active_before_pause.try_get::<i64, _>("attempt_generation")?)?;
    let active_dispatch_uid: Uuid = active_before_pause.try_get("active_dispatch_uid")?;
    let initial_generation: i64 = sqlx::query_scalar(
        "SELECT controller_generation FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    let initial_generation = u64::try_from(initial_generation)?;
    let pause_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: initial_generation,
    };
    let pause: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/pause", &pause_request)
        .await?;
    let paused_generation = match pause {
        ExecutionRunControlResponse::Applied {
            run: summary,
            controller_generation,
            ..
        } => {
            assert!(matches!(
                summary.status,
                ExecutionRunStatus::Pausing | ExecutionRunStatus::Paused
            ));
            assert_eq!(controller_generation, initial_generation + 1);
            controller_generation
        }
        other => bail!("pause was not applied: {other:?}"),
    };
    let cancelling: String = sqlx::query_scalar(
        "SELECT attempt_state FROM moa.execution_task \
         WHERE run_uid = $1 AND node_id = 'pausable-agent'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(cancelling, "cancelling");
    let cancel = sqlx::query(
        "SELECT controller_generation, attempt_generation, payload \
         FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND task_id = $2 \
           AND dispatch_kind = 'task_attempt_cancel'",
    )
    .bind(run.run_uid)
    .bind(task_id(&running).as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        u64::try_from(cancel.try_get::<i64, _>("controller_generation")?)?,
        paused_generation
    );
    assert_eq!(
        u64::try_from(cancel.try_get::<i64, _>("attempt_generation")?)?,
        attempt_generation
    );
    let cancel_payload: Value = cancel.try_get("payload")?;
    assert_eq!(
        cancel_payload
            .get("controller_generation")
            .and_then(Value::as_u64),
        Some(paused_generation),
        "pause cancellation payload used the pre-pause generation"
    );
    assert_eq!(
        cancel_payload
            .get("task_generation")
            .and_then(Value::as_u64),
        Some(task_generation)
    );
    assert_eq!(
        cancel_payload
            .get("attempt_generation")
            .and_then(Value::as_u64),
        Some(attempt_generation)
    );
    let active_dispatch_uid_string = active_dispatch_uid.to_string();
    assert_eq!(
        cancel_payload
            .get("active_dispatch_uid")
            .and_then(Value::as_str),
        Some(active_dispatch_uid_string.as_str())
    );
    controller.release(1);
    await_run_status(&test, &run, ExecutionRunStatus::Paused).await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    let drained = sqlx::query(
        "SELECT task.attempt_state, task.active_dispatch_uid, dispatch.state AS cancel_state, \
                capacity.state AS capacity_state, watchdog.state AS watchdog_state \
         FROM moa.execution_task AS task \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.run_uid = task.run_uid AND dispatch.task_id = task.task_id \
          AND dispatch.dispatch_kind = 'task_attempt_cancel' \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.run_uid = task.run_uid AND capacity.task_id = task.task_id \
          AND capacity.attempt_generation = $3 \
          AND capacity.resource_dimension = 'active_tasks' \
         JOIN moa.execution_trigger AS watchdog \
           ON watchdog.run_uid = task.run_uid AND watchdog.task_id = task.task_id \
          AND watchdog.attempt_generation = $3 AND watchdog.trigger_kind = 'task_watchdog' \
         WHERE task.run_uid = $1 AND task.task_id = $2",
    )
    .bind(run.run_uid)
    .bind(task_id(&running).as_uuid())
    .bind(i64::try_from(attempt_generation)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(drained.try_get::<String, _>("attempt_state")?, "idle");
    assert_eq!(
        drained.try_get::<Option<Uuid>, _>("active_dispatch_uid")?,
        None
    );
    assert_eq!(drained.try_get::<String, _>("cancel_state")?, "delivered");
    assert_eq!(drained.try_get::<String, _>("capacity_state")?, "released");
    assert_eq!(
        drained.try_get::<String, _>("watchdog_state")?,
        "superseded"
    );
    let pause_replay: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/pause", &pause_request)
        .await?;
    assert!(matches!(
        pause_replay,
        ExecutionRunControlResponse::Replayed {
            controller_generation,
            ..
        } if controller_generation == paused_generation
    ));

    let stale_resume: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/resume", &pause_request)
        .await?;
    assert_eq!(
        stale_resume,
        ExecutionRunControlResponse::Conflict {
            reason: ExecutionConflictReason::GenerationMismatch,
        }
    );
    let resume_request = ExecutionRunControlRequest {
        run: run.request.clone(),
        expected_controller_generation: paused_generation,
    };
    let resume: ExecutionRunControlResponse = test
        .client()
        .post_call("/Execution/resume", &resume_request)
        .await?;
    assert!(matches!(
        resume,
        ExecutionRunControlResponse::Applied {
            controller_generation,
            ..
        } if controller_generation == paused_generation + 1
    ));
    let second = controller.wait_for_calls(2, SCENARIO_TIMEOUT).await?;
    assert_eq!(second[0].input, second[1].input);
    assert_ne!(second[0].invocation_id, second[1].invocation_id);
    controller.release(1);
    let terminal = match await_run_status(&test, &run, ExecutionRunStatus::Completed).await {
        Ok(terminal) => terminal,
        Err(error) => {
            match active_pause_timeout_diagnostic(&pool, run.run_uid, task_id(&running)).await {
                Ok(diagnostic) => bail!("{error:#}; active_pause_diagnostic={diagnostic}"),
                Err(diagnostic_error) => bail!(
                    "{error:#}; active-pause diagnostic query also failed: {diagnostic_error:#}"
                ),
            }
        }
    };
    assert_eq!(terminal.output, Some(json!({"pause": "resumed"})));
    Ok(())
}

async fn active_pause_timeout_diagnostic(
    pool: &PgPool,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> Result<Value> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
             'run', (SELECT jsonb_build_object( \
                 'status', status, 'activation_state', activation_state, \
                 'controller_generation', controller_generation, 'wake_epoch', wake_epoch, \
                 'processed_wake_epoch', processed_wake_epoch, \
                 'ready_task_count', ready_task_count, 'active_task_count', active_task_count, \
                 'waiting_task_count', waiting_task_count) \
               FROM moa.execution_run WHERE run_uid=$1), \
             'task', (SELECT jsonb_build_object( \
                 'status', status, 'attempt_state', attempt_state, 'generation', generation, \
                 'attempt_generation', attempt_generation, \
                 'active_dispatch_uid', active_dispatch_uid) \
               FROM moa.execution_task WHERE run_uid=$1 AND task_id=$2), \
             'capacity', COALESCE((SELECT jsonb_agg(jsonb_build_object( \
                 'reservation_uid', reservation_uid, 'state', state, \
                 'attempt_generation', attempt_generation, \
                 'resource_dimension', resource_dimension) ORDER BY created_at) \
               FROM moa.execution_capacity_reservation \
               WHERE run_uid=$1 AND task_id=$2), '[]'::JSONB), \
             'watchdogs', COALESCE((SELECT jsonb_agg(jsonb_build_object( \
                 'trigger_uid', trigger_uid, 'state', state, \
                 'controller_generation', controller_generation, \
                 'attempt_generation', attempt_generation) ORDER BY created_at) \
               FROM moa.execution_trigger \
               WHERE run_uid=$1 AND task_id=$2 AND trigger_kind='task_watchdog'), '[]'::JSONB), \
             'activations', COALESCE((SELECT jsonb_agg(recent.value) FROM ( \
                 SELECT jsonb_build_object( \
                     'dispatch_uid', dispatch_uid, 'state', state, \
                     'controller_generation', controller_generation, 'wake_epoch', wake_epoch, \
                     'last_error', last_error) AS value \
                 FROM moa.execution_dispatch_outbox \
                 WHERE run_uid=$1 AND dispatch_kind='run_activation' \
                 ORDER BY created_at DESC LIMIT 5) AS recent), '[]'::JSONB))",
    )
    .bind(run_uid)
    .bind(task_id.as_uuid())
    .fetch_one(pool)
    .await
    .context("load active-pause timeout diagnostic")
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn parked_signal_rejects_wrong_generation_and_replays_duplicate_after_valkey_loss_service_e2e()
-> Result<()> {
    // Pins: a storage-only externally resumed wait owns no active compute,
    // rejects a wrong generation, and applies/replays the exact callback once
    // even when Valkey state is replaced before delivery.
    let fixture = execution_fixture(Vec::new()).await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "external-signal-fence",
        vec![
            node(
                "external-signal",
                &[],
                ExecutionOperation::WaitSignal {
                    signal_name: "provider-complete".to_string(),
                    wait_policy: ExecutionWaitPolicy {
                        expiry: after_logical_days(5),
                        on_expiry: ExecutionWaitExpiryAction::FailTask,
                    },
                },
                json!({"type": "object"}),
            ),
            output_node(&["external-signal"], json!({"external": "settled"})),
        ],
        Duration::from_secs(15),
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingSignal).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "external-signal",
        ExecutionTaskStatus::WaitingSignal,
    )
    .await?;
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;
    fixture.recreate_valkey_after_loss().await?;

    let stale: ExecutionMutationResponse = test
        .client()
        .post_call(
            "/Execution/deliver_signal",
            &ExecutionSignalRequest {
                tenant_id: run.tenant_id,
                contact_id: None,
                run_uid: run.run_uid,
                task_id: task_id(&waiting),
                expected_generation: waiting.generation + 1,
                signal_name: "provider-complete".to_string(),
                payload: json!({"callback": "late"}),
            },
        )
        .await?;
    assert_eq!(
        stale,
        ExecutionMutationResponse::Conflict {
            reason: ExecutionConflictReason::GenerationMismatch,
        }
    );

    let request = ExecutionSignalRequest {
        tenant_id: run.tenant_id,
        contact_id: None,
        run_uid: run.run_uid,
        task_id: task_id(&waiting),
        expected_generation: waiting.generation,
        signal_name: "provider-complete".to_string(),
        payload: json!({"callback": "current"}),
    };
    let applied: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/deliver_signal", &request)
        .await?;
    assert!(matches!(applied, ExecutionMutationResponse::Applied { .. }));
    let replay: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/deliver_signal", &request)
        .await?;
    assert!(matches!(replay, ExecutionMutationResponse::Replayed { .. }));

    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"external": "settled"})));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker for the real Restate/Postgres/Valkey execution fixture"]
async fn input_wait_releases_compute_and_resumes_same_attempt_under_new_generation_service_e2e()
-> Result<()> {
    // Pins: a model-authored user-input wait parks without active compute, then
    // resumes the same logical attempt under a new generation and replays input once.
    let needs_input = serde_json::to_string(&ExecutionTaskResult::NeedsInput {
        question: "Which source should be used?".to_string(),
        audience: InputAudience::User,
    })?;
    let completed = serde_json::to_string(&json!({"result": "used analyst notes"}))?;
    let fixture = execution_fixture_with_script(
        json!({
            "default": {"content": needs_input, "tool_calls": []},
            "keyed": [{
                "match": "analyst-notes",
                "completion": {"content": completed, "tool_calls": []}
            }]
        }),
        Vec::new(),
    )
    .await?;
    let test = fixture.isolated().await;
    let pool = PgPool::connect(&fixture.postgres_url).await?;
    let run = start_plan(
        &test,
        "input-wait",
        vec![
            node(
                "input-agent",
                &[],
                ExecutionOperation::Agent {
                    instructions: "Return a typed execution result.".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 2,
                },
                json!({"type": "object"}),
            ),
            output_node(&["input-agent"], json!({"input": "settled"})),
        ],
        Duration::from_secs(15),
    )
    .await?;
    await_run_status(&test, &run, ExecutionRunStatus::WaitingInput).await?;
    let waiting = await_task_status(
        &test,
        &run,
        "input-agent",
        ExecutionTaskStatus::WaitingInput,
    )
    .await?;
    assert_eq!(waiting.attempt, 1);
    assert_parked_has_no_active_compute(&fixture, &pool, &run).await?;

    let request = ExecutionInputRequest {
        tenant_id: run.tenant_id,
        contact_id: None,
        session_id: Some(run.request.session_id),
        run_uid: run.run_uid,
        task_id: task_id(&waiting),
        expected_generation: waiting.generation,
        audience: InputAudience::User,
        input: json!({"source": "analyst-notes"}),
    };
    let applied: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/deliver_input", &request)
        .await?;
    assert!(matches!(applied, ExecutionMutationResponse::Applied { .. }));
    let replay: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/deliver_input", &request)
        .await?;
    assert!(matches!(replay, ExecutionMutationResponse::Replayed { .. }));

    let completed_task =
        await_task_status(&test, &run, "input-agent", ExecutionTaskStatus::Completed).await?;
    assert_eq!(completed_task.attempt, 1);
    assert_eq!(completed_task.generation, waiting.generation + 1);
    let terminal = await_run_status(&test, &run, ExecutionRunStatus::Completed).await?;
    assert_eq!(terminal.output, Some(json!({"input": "settled"})));
    Ok(())
}

async fn await_active_execution_task_hand(
    pool: &PgPool,
    run: &StartedRun,
    expected_task_id: ExecutionTaskId,
) -> Result<Uuid> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let rows: Vec<Uuid> = sqlx::query_scalar(
            "SELECT workspace.workspace_id \
             FROM moa.sandbox_workspaces AS workspace \
             JOIN moa.sandbox_capacity_reservations AS reservation \
               ON reservation.tenant_id = workspace.tenant_id \
              AND reservation.workspace_id = workspace.workspace_id \
             WHERE workspace.scope_kind = 'execution_task' \
               AND workspace.scope_run_id = $1 AND workspace.scope_task_id = $2 \
               AND reservation.resource_dimension = 'active_hands' \
               AND reservation.reservation_state <> 'released'",
        )
        .bind(run.run_uid)
        .bind(expected_task_id.as_uuid())
        .fetch_all(pool)
        .await?;
        match rows.as_slice() {
            [workspace_id] => return Ok(*workspace_id),
            [] if Instant::now() < deadline => tokio::time::sleep(POLL_INTERVAL).await,
            [] => {
                bail!("execution task {expected_task_id} never acquired an ActiveHands reservation")
            }
            _ => bail!(
                "execution task {expected_task_id} acquired multiple live ActiveHands reservations: {rows:?}"
            ),
        }
    }
}

async fn await_execution_action_review(
    test: &IsolatedTest<'_>,
    tenant_id: TenantId,
    expected_task_id: ExecutionTaskId,
) -> Result<ActionReviewSummary> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let reviews: Vec<ActionReviewSummary> = test
            .client()
            .post_call(
                "/ActionReviews/list_pending",
                &ListActionReviewsRequest { tenant_id },
            )
            .await?;
        if let Some(review) = reviews.into_iter().find(|review| {
            review
                .envelope
                .owner
                .execution_origin()
                .is_some_and(|origin| origin.task_uid == expected_task_id.as_uuid())
        }) {
            assert_eq!(review.status, ActionReviewStatus::Pending);
            return Ok(review);
        }
        if Instant::now() >= deadline {
            bail!("task {expected_task_id} did not publish an action review")
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_execution_review_delivery(
    pool: &PgPool,
    review_uid: Uuid,
) -> Result<(i32, Option<String>)> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let delivery: Option<(i32, Option<DateTime<Utc>>, Option<String>)> = sqlx::query_as(
            "SELECT attempt_count, delivered_at, last_error \
             FROM moa.execution_action_review_outbox WHERE review_uid=$1",
        )
        .bind(review_uid)
        .fetch_optional(pool)
        .await?;
        if let Some((attempt_count, Some(_), last_error)) = &delivery {
            return Ok((*attempt_count, last_error.clone()));
        }
        if Instant::now() >= deadline {
            bail!(
                "execution review {review_uid} was not delivered within {SCENARIO_TIMEOUT:?}; delivery={delivery:?}"
            )
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn assert_released_execution_task_hand(
    pool: &PgPool,
    run: &StartedRun,
    expected_task_id: ExecutionTaskId,
    expected_workspace_id: Uuid,
) -> Result<()> {
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT reservation.reservation_state \
         FROM moa.sandbox_workspaces AS workspace \
         JOIN moa.sandbox_capacity_reservations AS reservation \
           ON reservation.tenant_id = workspace.tenant_id \
          AND reservation.workspace_id = workspace.workspace_id \
         WHERE workspace.scope_kind = 'execution_task' \
           AND workspace.scope_run_id = $1 AND workspace.scope_task_id = $2 \
           AND workspace.workspace_id = $3 \
           AND reservation.resource_dimension = 'active_hands'",
    )
    .bind(run.run_uid)
    .bind(expected_task_id.as_uuid())
    .bind(expected_workspace_id)
    .fetch_all(pool)
    .await?;
    assert_eq!(
        states,
        vec!["released".to_string()],
        "the exact execution-task ActiveHands receipt must be released while compute is parked"
    );
    Ok(())
}
