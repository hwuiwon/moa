//! Durable compensation-attempt lifecycle behavior.

use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, ExecutionCompensation,
};
use moa_config::ExecutionConfig;
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    tools::{AsyncToolJob, IdempotencyClass},
};
use moa_execution::{
    capability::{
        CapabilityPolicyContext, CapabilityRollbackContract, CapabilitySource, ExecutionCapability,
        ExecutionClass,
    },
    repository::{
        compensation::{
            CompensationAttemptAdmission, CompensationAttemptAdmissionOutcome,
            CompensationAttemptFence, CompensationAttemptReleaseClaimOutcome,
            CompensationAttemptState, CompensationAttemptWriteOutcome,
            CompensationReviewResolutionOutcome,
        },
        external_job::{
            ExecutionExternalJobBinding, ExecutionExternalJobCallback,
            ExecutionExternalJobCallbackOutcome, ExecutionExternalJobCallbackUpdate,
            ExecutionExternalJobStartRecoveryAdoptionOutcome, NewExecutionExternalJobIntent,
        },
        external_job::{ExecutionExternalJobOwner, ExecutionExternalJobState},
        terminal::{PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage},
    },
    state::{CompensationStatus, ExecutionCompensationOutcome, ExecutionTerminalEvidence},
    wire::{
        ExecutionCompensationAttemptCancelRequest, ExecutionCompensationReleaseIntent,
        ExecutionExternalJobStartRecoveryOwner, ExecutionExternalJobStartRecoveryRequest,
    },
};

use super::support::*;
use std::time::Duration as StdDuration;

#[tokio::test]
async fn compensation_admission_locks_watchdog_capacity_before_run_db() -> TestResult {
    // Pins: compensation admission reserves both active-task and watchdog capacity before it
    // locks the run. A settlement that already owns ScheduledTriggers can therefore retain the
    // run lock without forming a run-to-capacity cycle with compensation admission.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["lock-order"]).await?;
    let run_uid = run.run_uid;
    let config = ExecutionConfig::default();

    let mut capacity_holder = pool.begin().await?;
    let locked: Vec<Uuid> = sqlx::query_scalar(
        "SELECT capacity_bucket_uid FROM moa.execution_capacity_bucket \
         WHERE resource_dimension='scheduled_triggers' \
           AND ((scope_kind='fleet' AND tenant_id IS NULL) \
             OR (scope_kind='tenant' AND tenant_id=$1)) \
         ORDER BY CASE scope_kind WHEN 'fleet' THEN 0 ELSE 1 END FOR UPDATE",
    )
    .bind(tenant_id.0)
    .fetch_all(&mut *capacity_holder)
    .await?;
    assert_eq!(locked.len(), 2);

    let admission_repository = repository.clone();
    let admission_config = config.clone();
    let mut admission = tokio::spawn(async move {
        admission_repository
            .admit_next_compensation_attempt(
                scope,
                &admission_config,
                run_uid,
                moa_test_support::fixtures::pg_now(),
            )
            .await
    });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(100), &mut admission)
            .await
            .is_err(),
        "admission must wait for ScheduledTriggers capacity"
    );
    sqlx::query("SELECT run_uid FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE NOWAIT")
        .bind(run_uid)
        .fetch_one(&mut *capacity_holder)
        .await?;
    capacity_holder.commit().await?;

    assert!(matches!(
        tokio::time::timeout(StdDuration::from_secs(5), admission).await???,
        CompensationAttemptAdmissionOutcome::Admitted(_)
            | CompensationAttemptAdmissionOutcome::Replayed(_)
    ));
    Ok(())
}

#[tokio::test]
async fn nonterminal_guard_uses_partial_index_for_more_than_2500_tasks_db() -> TestResult {
    // Pins: compensation admission's forward-work guard remains an indexed existence probe even
    // when a run retains thousands of terminal task rows and only one nonterminal task remains.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::ControlPlane;
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "compensation-nonterminal-index",
            ExecutionRunStatus::Queued,
            budget(3_000),
        ),
    )
    .await?;
    const TASK_COUNT: usize = 2_501;
    const PAGE_SIZE: usize = 1_000;
    const LIVE_ITEM_KEY: &str = "item-02500";
    for page_start in (0..TASK_COUNT).step_by(PAGE_SIZE) {
        let page_end = (page_start + PAGE_SIZE).min(TASK_COUNT);
        let tasks = (page_start..page_end)
            .map(|index| {
                logical_task(
                    run.run_uid,
                    "large-terminal-history",
                    &format!("item-{index:05}"),
                    estimate(1),
                )
            })
            .collect::<Vec<_>>();
        repository
            .materialize_tasks(scope, run.run_uid, run.plan_revision, tasks)
            .await?;
    }
    let settled = sqlx::query(
        "UPDATE moa.execution_task SET status='skipped',attempt_state='terminal', \
             completed_at=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND item_key<>$2",
    )
    .bind(run.run_uid)
    .bind(LIVE_ITEM_KEY)
    .execute(test_db.store().pool())
    .await?;
    assert_eq!(settled.rows_affected(), 2_500);

    let mut transaction = test_db.store().pool().begin().await?;
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(transaction.as_mut())
        .await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome'))",
    )
    .bind(run.run_uid)
    .fetch_one(transaction.as_mut())
    .await?;
    assert!(
        exists,
        "the one pending task must block compensation admission"
    );
    let explain: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (ANALYZE, COSTS OFF, FORMAT JSON) \
         SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome'))",
    )
    .bind(run.run_uid)
    .fetch_one(transaction.as_mut())
    .await?;
    let scan = explain_index_scan(&explain, "execution_task_nonterminal_run_idx")
        .expect("nonterminal existence probe must use its partial run index");
    assert_eq!(
        scan.get("Actual Loops").and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        scan.get("Actual Rows").and_then(serde_json::Value::as_u64),
        Some(1)
    );
    transaction.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_admission_replays_only_highest_reverse_order_slice_db() -> TestResult {
    // Pins: after the bounded pending-terminal page admits the highest reverse registration,
    // competing retries replay that exact slice without dispatching a lower registration.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, registrations) =
        compensating_run(&repository, scope, tenant_id, &["first", "second"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();

    let (left, right) = tokio::join!(
        repository.admit_next_compensation_attempt(scope, &config, run.run_uid, now),
        repository.admit_next_compensation_attempt(scope, &config, run.run_uid, now),
    );
    let outcomes = [left?, right?];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CompensationAttemptAdmissionOutcome::Replayed(_)))
            .count(),
        2
    );
    for outcome in outcomes {
        let admission = match outcome {
            CompensationAttemptAdmissionOutcome::Admitted(admission)
            | CompensationAttemptAdmissionOutcome::Replayed(admission) => admission,
            other => panic!("concurrent admission returned {other:?}"),
        };
        assert_eq!(
            admission.attempt.registration.compensation_id, registrations[0].compensation_id,
            "only the highest registered sequence may dispatch"
        );
        assert_eq!(
            admission.attempt.attempt_state,
            CompensationAttemptState::Dispatching
        );
        assert_eq!(admission.attempt.run, run);
        assert_eq!(
            admission.attempt.active_dispatch_uid,
            Some(admission.dispatch.dispatch_uid)
        );
    }
    let dispatch_rows: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM moa.execution_dispatch_outbox WHERE run_uid=$1 \
         AND compensation_id=$2 AND dispatch_kind='compensation_attempt'",
    )
    .bind(run.run_uid)
    .bind(registrations[0].compensation_id.as_uuid())
    .fetch_all(test_db.store().pool())
    .await?;
    assert_eq!(dispatch_rows.len(), 1);
    let payload = dispatch_rows.first().expect("one compensation payload");
    assert!(payload.get("identity").is_none());
    assert!(payload.get("contact_id").is_none());
    assert!(payload.get("session_id").is_none());
    let expected_compensation_id = registrations[0].compensation_id.to_string();
    assert_eq!(
        payload
            .get("compensation_id")
            .and_then(serde_json::Value::as_str),
        Some(expected_compensation_id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn paused_slice_releases_capacity_only_after_verified_teardown_db() -> TestResult {
    // Pins: pause first makes the slice non-dispatchable while retaining capacity, then the
    // verified release finalizer returns the logical effect to idle without changing its generation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["effect"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    let CompensationAttemptWriteOutcome::Applied(started) = repository
        .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
        .await?
    else {
        panic!("admitted compensation slice must start");
    };
    assert_eq!(started.attempt_state, CompensationAttemptState::Running);
    let release_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Pause,
    );
    let CompensationAttemptReleaseClaimOutcome::Applied(claimed) = repository
        .begin_compensation_attempt_release(&release_request, now + Duration::milliseconds(2))
        .await?
    else {
        panic!("pause must claim the active compensation before provider release");
    };
    assert_eq!(claimed.attempt_state, CompensationAttemptState::Cancelling);
    assert_eq!(
        claimed.release_intent,
        Some(ExecutionCompensationReleaseIntent::Pause)
    );
    let reservation_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation WHERE reservation_uid=$1",
    )
    .bind(admission.capacity_reservation_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(reservation_state, "reserved");
    assert!(matches!(
        repository
            .yield_released_compensation_attempt(
                &release_request,
                now + Duration::milliseconds(3),
                None,
            )
            .await?,
        CompensationAttemptWriteOutcome::Conflict
    ));
    let release_receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    let CompensationAttemptWriteOutcome::Applied(yielded) = repository
        .yield_released_compensation_attempt(
            &release_request,
            now + Duration::milliseconds(3),
            Some(release_receipt),
        )
        .await?
    else {
        panic!("active compensation slice must yield");
    };
    assert_eq!(
        yielded.registration.generation,
        fence.compensation_generation
    );
    assert_eq!(yielded.attempt_generation, fence.attempt_generation + 1);
    assert_eq!(yielded.attempt_state, CompensationAttemptState::Idle);
    assert_eq!(yielded.active_dispatch_uid, None);
    assert_eq!(yielded.release_intent, None);
    let reservation_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation WHERE reservation_uid=$1",
    )
    .bind(admission.capacity_reservation_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(reservation_state, "released");
    let next = active_compensation_admission(
        &repository,
        scope,
        &ExecutionConfig::default(),
        run.run_uid,
        now + Duration::milliseconds(4),
    )
    .await?;
    assert_eq!(
        next.attempt.registration.generation,
        fence.compensation_generation
    );
    assert_eq!(
        next.attempt.attempt_generation,
        fence.attempt_generation + 1
    );
    assert_ne!(next.dispatch.dispatch_uid, fence.dispatch_uid);
    Ok(())
}

#[tokio::test]
async fn journaled_compensation_release_time_cannot_regress_progress_db() -> TestResult {
    // Pins: a Restate-journaled release time may predate a later database progress write; the
    // exact fenced teardown still applies while the durable progress watermark stays monotonic.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) =
        compensating_run(&repository, scope, tenant_id, &["stale-journal-clock"]).await?;
    let claim_owner = "compensation-clock-pin";
    let claimed = repository
        .claim_due_dispatches(scope, claim_owner, 100, StdDuration::from_secs(30))
        .await?;
    let controller_wakes = claimed
        .iter()
        .filter(|dispatch| {
            dispatch.kind == moa_execution::repository::outbox::ExecutionDispatchKind::RunActivation
                && dispatch.run_uid == Some(run.run_uid)
                && dispatch.controller_generation == Some(run.controller_generation)
                && dispatch.wake_epoch == Some(run.wake_epoch)
        })
        .collect::<Vec<_>>();
    assert_eq!(controller_wakes.len(), 1);
    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &[controller_wakes[0].dispatch_uid], claim_owner,)
            .await?,
        vec![controller_wakes[0].dispatch_uid]
    );
    let journaled_at = moa_test_support::fixtures::pg_now();
    let admission = active_compensation_admission(
        &repository,
        scope,
        &ExecutionConfig::default(),
        run.run_uid,
        journaled_at,
    )
    .await?;
    let fence = fence(&admission);
    let CompensationAttemptWriteOutcome::Applied(started) = repository
        .start_compensation_attempt(scope, fence, journaled_at + Duration::milliseconds(1))
        .await?
    else {
        panic!("admitted compensation slice must start");
    };
    let database_progress_at = journaled_at + Duration::seconds(1);
    sqlx::query(
        "UPDATE moa.execution_compensation SET last_progress_at=$3,updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2",
    )
    .bind(run.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .bind(database_progress_at)
    .execute(test_db.store().pool())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET last_progress_at=$2,updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .bind(database_progress_at)
    .execute(test_db.store().pool())
    .await?;

    let release_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Pause,
    );
    let CompensationAttemptReleaseClaimOutcome::Applied(claimed) = repository
        .begin_compensation_attempt_release(
            &release_request,
            journaled_at + Duration::milliseconds(2),
        )
        .await?
    else {
        panic!("stale journal clock must not reject the exact release claim");
    };
    assert_eq!(claimed.attempt_state, CompensationAttemptState::Cancelling);
    assert_eq!(claimed.last_progress_at, database_progress_at);
    assert!(claimed.last_progress_at > started.last_progress_at);

    let receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        journaled_at + Duration::milliseconds(3),
    )
    .await?;
    let CompensationAttemptWriteOutcome::Applied(released) = repository
        .yield_released_compensation_attempt(
            &release_request,
            journaled_at + Duration::milliseconds(3),
            Some(receipt),
        )
        .await?
    else {
        panic!("stale journal clock must not reject the exact release finalizer");
    };
    assert_eq!(released.attempt_state, CompensationAttemptState::Idle);
    assert_eq!(released.last_progress_at, database_progress_at);
    let released_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("released compensation run remains visible");
    assert_eq!(released_run.last_progress_at, database_progress_at);
    assert_eq!(released_run.wake_epoch, run.wake_epoch + 1);
    Ok(())
}

#[tokio::test]
async fn running_compensation_pause_drains_exact_attempt_then_resumes_once_db() -> TestResult {
    // Pins: pausing a compensating run fences the run at G+1 while cancellation retains the
    // attempt's G resource owner; only verified teardown reaches Paused, and resume restores the
    // original pending terminal intent with exactly one activation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["running-pause"]).await?;
    let pending_terminal = run
        .pending_terminal
        .clone()
        .expect("compensating fixture must preserve its terminal intent");
    let now = moa_test_support::fixtures::pg_now();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));

    let TransitionOutcome::RunApplied(pausing) = repository
        .pause_run(scope, &config, run.run_uid, run.controller_generation)
        .await?
    else {
        panic!("running compensation pause must install its cancellation fence");
    };
    assert_eq!(pausing.status, ExecutionRunStatus::Pausing);
    assert_eq!(pausing.controller_generation, run.controller_generation + 1);
    assert_eq!(pausing.active_task_count, 1);
    assert_eq!(pausing.pending_terminal, Some(pending_terminal.clone()));
    let cancellation_payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM moa.execution_dispatch_outbox WHERE run_uid=$1 \
         AND compensation_id=$2 AND dispatch_kind='compensation_attempt_cancel'",
    )
    .bind(run.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    let cancellation: ExecutionCompensationAttemptCancelRequest =
        serde_json::from_value(cancellation_payload)?;
    assert_eq!(
        cancellation.controller_generation,
        pausing.controller_generation
    );
    assert_eq!(
        cancellation.attempt_controller_generation,
        run.controller_generation
    );
    assert_eq!(
        cancellation.intent,
        ExecutionCompensationReleaseIntent::Pause
    );
    let cancelling_state: String = sqlx::query_scalar(
        "SELECT attempt_state FROM moa.execution_compensation \
         WHERE run_uid=$1 AND compensation_id=$2",
    )
    .bind(run.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(cancelling_state, "cancelling");

    let release_receipt =
        persist_compensation_release_receipt(&pool, &cancellation, now + Duration::milliseconds(2))
            .await?;
    assert!(matches!(
        repository
            .yield_released_compensation_attempt(
                &cancellation,
                now + Duration::milliseconds(3),
                Some(release_receipt),
            )
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let paused = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("drained compensation run remains visible");
    assert_eq!(paused.status, ExecutionRunStatus::Paused);
    assert_eq!(paused.activation_state, ExecutionActivationState::Paused);
    assert_eq!(paused.active_task_count, 0);
    assert_eq!(paused.pending_terminal, Some(pending_terminal.clone()));
    assert_eq!(
        run_activation_count(&pool, paused.run_uid, paused.controller_generation).await?,
        0,
        "provider teardown must not wake a Pausing or Paused run"
    );

    let TransitionOutcome::RunApplied(resumed) = repository
        .resume_run(scope, &config, paused.run_uid, paused.controller_generation)
        .await?
    else {
        panic!("fully drained compensation run must resume");
    };
    assert_eq!(resumed.status, ExecutionRunStatus::Compensating);
    assert_eq!(resumed.pending_terminal, Some(pending_terminal));
    assert_eq!(
        run_activation_count(&pool, resumed.run_uid, resumed.controller_generation).await?,
        1
    );
    assert!(matches!(
        repository
            .resume_run(scope, &config, paused.run_uid, paused.controller_generation,)
            .await?,
        TransitionOutcome::RunAlreadyApplied(_)
    ));
    assert_eq!(
        run_activation_count(&pool, resumed.run_uid, resumed.controller_generation).await?,
        1,
        "resume replay must not enqueue a second activation"
    );
    Ok(())
}

#[tokio::test]
async fn dispatch_delivery_loss_releases_never_started_compensation_for_retry_db() -> TestResult {
    // Pins: a dispatching compensation owns capacity/watchdog but no provider hand; its exact
    // verified no-hand receipt releases both and creates one fresh admissible attempt generation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["delivery-lost"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    assert_eq!(
        admission.attempt.attempt_state,
        CompensationAttemptState::Dispatching
    );
    let request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Watchdog,
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&request, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptReleaseClaimOutcome::Applied(_)
    ));
    let receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &request,
        now + Duration::milliseconds(2),
    )
    .await?;
    let CompensationAttemptWriteOutcome::Applied(retry) = repository
        .settle_released_compensation_attempt(
            &request,
            ExecutionCompensationOutcome::Failed {
                message: "dispatch delivery lost before provider start".to_string(),
                retryable: true,
                usage: usage(0),
            },
            now + Duration::milliseconds(2),
            Some(receipt),
        )
        .await?
    else {
        panic!("verified never-started attempt must requeue");
    };
    assert_eq!(retry.attempt_state, CompensationAttemptState::Idle);
    assert_eq!(
        retry.attempt_generation,
        admission.attempt.attempt_generation + 1
    );
    let reservation_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation WHERE reservation_uid=$1",
    )
    .bind(admission.capacity_reservation_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(reservation_state, "released");
    let active_watchdogs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_trigger WHERE trigger_uid=$1 \
         AND state = 'pending'",
    )
    .bind(admission.watchdog.trigger.trigger_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(active_watchdogs, 0);
    assert!(matches!(
        repository
            .admit_next_compensation_attempt(
                scope,
                &config,
                run.run_uid,
                now + Duration::milliseconds(3),
            )
            .await?,
        CompensationAttemptAdmissionOutcome::Admitted(_)
    ));
    Ok(())
}

#[tokio::test]
async fn recovered_not_started_compensation_requires_verified_release_before_retry_db() -> TestResult
{
    // Pins: provider NotStarted proof releases the exact external intent and atomically fences
    // the running compensation as Retry; ActiveTasks/watchdog remain owned until a persisted
    // verified hand receipt advances one fresh Idle attempt generation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["not-started"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let external_job_uid = Uuid::now_v7();
    let provider = "recovery-provider".to_string();
    let idempotency_key = format!("recovery-not-started-{}", Uuid::now_v7());
    let owner = ExecutionExternalJobOwner::Compensation {
        compensation_id: fence.compensation_id.as_uuid(),
        compensation_generation: fence.compensation_generation,
        compensation_attempt_generation: fence.attempt_generation,
    };
    repository
        .reserve_external_job_intent(
            scope,
            &config,
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                provider: provider.clone(),
                idempotency_key: idempotency_key.clone(),
                expires_at: now + Duration::minutes(1),
            },
        )
        .await?;
    let recovery = ExecutionExternalJobStartRecoveryRequest {
        tenant_id,
        run_uid: run.run_uid,
        owner: ExecutionExternalJobStartRecoveryOwner::Compensation {
            compensation_id: fence.compensation_id.as_uuid(),
            compensation_generation: fence.compensation_generation,
            compensation_attempt_generation: fence.attempt_generation,
        },
        external_job_uid,
        job_generation: 1,
        provider,
        idempotency_key,
        trigger_uid: Uuid::now_v7(),
    };
    let ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
        compensation_release: Some(release_request),
    } = repository
        .recover_external_job_start_not_started(&recovery, now + Duration::milliseconds(2))
        .await?
    else {
        panic!("NotStarted recovery must fence exact compensation teardown");
    };
    assert_eq!(
        release_request.intent,
        ExecutionCompensationReleaseIntent::Retry
    );
    assert!(
        repository
            .load_external_job(scope, external_job_uid)
            .await?
            .is_none()
    );
    assert!(matches!(
        repository
            .yield_released_compensation_attempt_after_external_not_started(
                &release_request,
                now + Duration::milliseconds(3),
                None,
            )
            .await?,
        CompensationAttemptWriteOutcome::Conflict
    ));
    let receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    let CompensationAttemptWriteOutcome::Applied(requeued) = repository
        .yield_released_compensation_attempt_after_external_not_started(
            &release_request,
            now + Duration::milliseconds(3),
            Some(receipt.clone()),
        )
        .await?
    else {
        panic!("verified NotStarted teardown must return the slice to Idle");
    };
    assert_eq!(requeued.attempt_state, CompensationAttemptState::Idle);
    assert_eq!(requeued.attempt_generation, fence.attempt_generation + 1);
    assert!(matches!(
        repository
            .yield_released_compensation_attempt_after_external_not_started(
                &release_request,
                now + Duration::milliseconds(4),
                Some(receipt),
            )
            .await?,
        CompensationAttemptWriteOutcome::Replayed(_)
    ));
    Ok(())
}

#[tokio::test]
async fn recovered_started_compensation_adopts_canonical_resources_before_external_wait_db()
-> TestResult {
    // Pins: provider Started recovery binds the job and derives the exact immutable compensation
    // dispatch/capacity/watchdog request from locked storage; it cannot park WaitingExternal until
    // the persisted verified hand-release receipt releases active ownership.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["started"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let external_job_uid = Uuid::now_v7();
    let provider = "recovered-start-provider".to_string();
    let idempotency_key = format!("recovery-started-{}", Uuid::now_v7());
    let owner = ExecutionExternalJobOwner::Compensation {
        compensation_id: fence.compensation_id.as_uuid(),
        compensation_generation: fence.compensation_generation,
        compensation_attempt_generation: fence.attempt_generation,
    };
    repository
        .reserve_external_job_intent(
            scope,
            &config,
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                provider: provider.clone(),
                idempotency_key: idempotency_key.clone(),
                expires_at: now + Duration::minutes(1),
            },
        )
        .await?;
    let recovery = ExecutionExternalJobStartRecoveryRequest {
        tenant_id,
        run_uid: run.run_uid,
        owner: ExecutionExternalJobStartRecoveryOwner::Compensation {
            compensation_id: fence.compensation_id.as_uuid(),
            compensation_generation: fence.compensation_generation,
            compensation_attempt_generation: fence.attempt_generation,
        },
        external_job_uid,
        job_generation: 1,
        provider: provider.clone(),
        idempotency_key: idempotency_key.clone(),
        trigger_uid: Uuid::now_v7(),
    };
    let binding = ExecutionExternalJobBinding {
        external_job_uid,
        tenant_id,
        run_uid: run.run_uid,
        owner,
        job_generation: 1,
        idempotency_key,
        provider,
        provider_job_id: format!("provider-job-{}", Uuid::now_v7()),
        callback_auth_reference: "vault://recovered-start".to_string(),
        state: ExecutionExternalJobState::Running,
        progress_phase: Some("running".to_string()),
        cancel_supported: true,
        next_reconcile_at: Some(now + Duration::minutes(2)),
        provider_contract_violation: None,
    };
    let ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
        compensation_release: Some(release_request),
    } = repository
        .recover_external_job_start_started(
            &config,
            &recovery,
            binding,
            now + Duration::milliseconds(2),
        )
        .await?
    else {
        panic!("Started recovery must adopt exact compensation ownership");
    };
    assert_eq!(
        release_request.capacity_reservation_uid,
        admission.capacity_reservation_uid
    );
    assert_eq!(
        release_request.watchdog_trigger_uid,
        admission.watchdog.trigger.trigger_uid
    );
    assert_eq!(
        release_request.intent,
        ExecutionCompensationReleaseIntent::ExternalJob
    );
    let receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    let waiting = repository
        .yield_released_compensation_attempt_to_external_job(
            &release_request,
            external_job_uid,
            Some(receipt),
            now + Duration::milliseconds(3),
        )
        .await?;
    assert!(matches!(
        waiting,
        moa_execution::repository::compensation::CompensationAttemptExternalOutcome::Applied {
            ref attempt,
            ..
        } if attempt.attempt_state == CompensationAttemptState::WaitingExternal
    ));
    Ok(())
}

#[tokio::test]
async fn review_resolution_requires_exact_uid_and_slice_generation_db() -> TestResult {
    // Pins: a stale or mismatched review callback cannot settle a newer
    // compensation slice, while the exact review can resolve once.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["reviewed"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let review_uid = Uuid::now_v7();
    let cancel_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Review,
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&cancel_request, now + Duration::milliseconds(2),)
            .await?,
        CompensationAttemptReleaseClaimOutcome::Applied(_)
    ));
    let release_receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &cancel_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    assert!(matches!(
        repository
            .park_released_compensation_review(
                &cancel_request,
                review_uid,
                now + Duration::minutes(5),
                now + Duration::milliseconds(3),
                Some(release_receipt),
            )
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let resolution = ExecutionActionReviewResolution::Completed {
        tool_output: json!({"undone": true}),
    };
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                run.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                Uuid::now_v7(),
                &resolution,
                now + Duration::milliseconds(4),
            )
            .await?,
        CompensationReviewResolutionOutcome::Stale
    ));
    let CompensationReviewResolutionOutcome::Applied(settled) = repository
        .resolve_current_compensation_review(
            scope,
            run.run_uid,
            fence.compensation_id,
            fence.compensation_generation,
            review_uid,
            &resolution,
            now + Duration::milliseconds(5),
        )
        .await?
    else {
        panic!("exact review must settle the parked compensation");
    };
    assert_eq!(settled.attempt_state, CompensationAttemptState::Terminal);
    assert_eq!(
        settled.registration.status,
        moa_execution::state::CompensationStatus::Completed
    );
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                run.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                review_uid,
                &resolution,
                now + Duration::milliseconds(6),
            )
            .await?,
        CompensationReviewResolutionOutcome::Replayed(_)
    ));
    Ok(())
}

#[tokio::test]
async fn paused_compensation_review_decision_waits_for_resume_activation_db() -> TestResult {
    // Pins: pausing after a compensation review parks must not stale its pre-pause attempt owner;
    // the exact decision persists while paused and only resume creates a controller activation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let (paused, fence, review_uid) = park_compensation_review_then_pause(
        &repository,
        test_db.store().pool(),
        scope,
        tenant_id,
        &config,
    )
    .await?;
    let resolved_at = moa_test_support::fixtures::pg_now() + Duration::seconds(1);
    let resolution = ExecutionActionReviewResolution::Completed {
        tool_output: json!({"undone": true}),
    };

    let CompensationReviewResolutionOutcome::Applied(settled) = repository
        .resolve_current_compensation_review(
            scope,
            paused.run_uid,
            fence.compensation_id,
            fence.compensation_generation,
            review_uid,
            &resolution,
            resolved_at,
        )
        .await?
    else {
        panic!("the exact pre-pause compensation review owner must remain resolvable");
    };
    assert_eq!(settled.attempt_state, CompensationAttemptState::Terminal);
    assert_eq!(
        settled.registration.status,
        moa_execution::state::CompensationStatus::Completed
    );
    let still_paused = repository
        .load_run(scope, paused.run_uid)
        .await?
        .expect("paused compensation run remains visible");
    assert_eq!(still_paused.status, ExecutionRunStatus::Paused);
    assert_eq!(
        still_paused.controller_generation,
        paused.controller_generation
    );
    assert_eq!(
        run_activation_count(
            test_db.store().pool(),
            paused.run_uid,
            paused.controller_generation,
        )
        .await?,
        0,
        "storage-only review resolution must not wake a paused run"
    );
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                paused.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                review_uid,
                &resolution,
                resolved_at + Duration::milliseconds(1),
            )
            .await?,
        CompensationReviewResolutionOutcome::Replayed(_)
    ));

    let TransitionOutcome::RunApplied(resumed) = repository
        .resume_run(scope, &config, paused.run_uid, paused.controller_generation)
        .await?
    else {
        panic!("paused compensation must resume after its decision is persisted");
    };
    assert_eq!(resumed.status, ExecutionRunStatus::Compensating);
    assert_eq!(
        run_activation_count(
            test_db.store().pool(),
            resumed.run_uid,
            resumed.controller_generation,
        )
        .await?,
        1,
        "resume must enqueue exactly one controller activation"
    );
    Ok(())
}

#[tokio::test]
async fn paused_compensation_review_timeout_waits_for_resume_activation_db() -> TestResult {
    // Pins: an authoritative timeout received while paused settles the exact parked review once,
    // remains storage-only, and is observed by the single activation created on resume.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let (paused, fence, review_uid) = park_compensation_review_then_pause(
        &repository,
        test_db.store().pool(),
        scope,
        tenant_id,
        &config,
    )
    .await?;
    let timed_out_at = moa_test_support::fixtures::pg_now() + Duration::seconds(1);
    let resolution = ExecutionActionReviewResolution::TimedOut {
        reason: "review expired".to_string(),
    };

    let CompensationReviewResolutionOutcome::Applied(settled) = repository
        .resolve_current_compensation_review(
            scope,
            paused.run_uid,
            fence.compensation_id,
            fence.compensation_generation,
            review_uid,
            &resolution,
            timed_out_at,
        )
        .await?
    else {
        panic!("the paused compensation review timeout must consume its exact parked owner");
    };
    assert_eq!(settled.attempt_state, CompensationAttemptState::Terminal);
    assert_eq!(
        settled.registration.status,
        moa_execution::state::CompensationStatus::Failed
    );
    assert_eq!(
        run_activation_count(
            test_db.store().pool(),
            paused.run_uid,
            paused.controller_generation,
        )
        .await?,
        0,
        "timeout settlement must not activate a paused run"
    );
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                paused.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                review_uid,
                &resolution,
                timed_out_at + Duration::milliseconds(1),
            )
            .await?,
        CompensationReviewResolutionOutcome::Replayed(_)
    ));

    let TransitionOutcome::RunApplied(resumed) = repository
        .resume_run(scope, &config, paused.run_uid, paused.controller_generation)
        .await?
    else {
        panic!("paused compensation must resume after its timeout is persisted");
    };
    assert_eq!(resumed.status, ExecutionRunStatus::Compensating);
    assert_eq!(
        run_activation_count(
            test_db.store().pool(),
            resumed.run_uid,
            resumed.controller_generation,
        )
        .await?,
        1,
        "resume must enqueue exactly one controller activation"
    );
    Ok(())
}

#[tokio::test]
async fn reviewed_external_job_is_parked_atomically_after_review_db() -> TestResult {
    // Pins: a decision arriving before the compensation is parked remains retryable, while an
    // exact approved async result atomically accepts the review and installs its durable job owner.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["reviewed-external"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let review_uid = Uuid::now_v7();
    let async_job = AsyncToolJob {
        provider: "review-provider".to_string(),
        provider_job_id: format!("provider-job-{}", Uuid::now_v7()),
        idempotency_key: format!("idempotency-{}", Uuid::now_v7()),
        callback_auth_reference: "vault://review-callback".to_string(),
        progress_phase: "queued".to_string(),
        cancel_supported: true,
        next_reconcile_at: now + Duration::minutes(2),
    };
    let external_job_uid = Uuid::now_v7();
    let resolution = ExecutionActionReviewResolution::ExternalJob {
        external_job_uid,
        job: async_job.clone(),
    };
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                run.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                review_uid,
                &resolution,
                now + Duration::milliseconds(2),
            )
            .await?,
        CompensationReviewResolutionOutcome::NotReady
    ));
    let release_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Review,
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&release_request, now + Duration::milliseconds(3),)
            .await?,
        CompensationAttemptReleaseClaimOutcome::Applied(_)
    ));
    let release_receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        now + Duration::milliseconds(4),
    )
    .await?;
    assert!(matches!(
        repository
            .park_released_compensation_review(
                &release_request,
                review_uid,
                now + Duration::minutes(5),
                now + Duration::milliseconds(4),
                Some(release_receipt),
            )
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let owner = ExecutionExternalJobOwner::Compensation {
        compensation_id: fence.compensation_id.as_uuid(),
        compensation_generation: fence.compensation_generation,
        compensation_attempt_generation: fence.attempt_generation,
    };
    repository
        .reserve_external_job_intent(
            scope,
            &config,
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                provider: async_job.provider.clone(),
                idempotency_key: async_job.idempotency_key.clone(),
                expires_at: now + Duration::minutes(1),
            },
        )
        .await?;
    repository
        .bind_external_job(
            scope,
            &config,
            ExecutionExternalJobBinding {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                idempotency_key: async_job.idempotency_key.clone(),
                provider: async_job.provider.clone(),
                provider_job_id: async_job.provider_job_id.clone(),
                callback_auth_reference: async_job.callback_auth_reference.clone(),
                state: ExecutionExternalJobState::Running,
                progress_phase: Some(async_job.progress_phase.clone()),
                cancel_supported: async_job.cancel_supported,
                provider_contract_violation: None,
                next_reconcile_at: Some(async_job.next_reconcile_at),
            },
        )
        .await?;
    let CompensationReviewResolutionOutcome::Applied(waiting) = repository
        .resolve_current_compensation_review(
            scope,
            run.run_uid,
            fence.compensation_id,
            fence.compensation_generation,
            review_uid,
            &resolution,
            now + Duration::milliseconds(5),
        )
        .await?
    else {
        panic!("exact reviewed async job must be durably parked");
    };
    assert_eq!(
        waiting.attempt_state,
        CompensationAttemptState::WaitingExternal
    );
    let persisted_external_job_uid = waiting
        .external_job_uid
        .expect("waiting external compensation must name its exact job");
    assert_eq!(persisted_external_job_uid, external_job_uid);
    let external_job = repository
        .load_external_job(scope, persisted_external_job_uid)
        .await?
        .expect("review resolution must persist the external job atomically");
    assert_eq!(external_job.state, ExecutionExternalJobState::Running);
    assert_eq!(
        external_job.owner,
        ExecutionExternalJobOwner::Compensation {
            compensation_id: fence.compensation_id.as_uuid(),
            compensation_generation: fence.compensation_generation,
            compensation_attempt_generation: fence.attempt_generation,
        }
    );
    assert!(matches!(
        repository
            .resolve_current_compensation_review(
                scope,
                run.run_uid,
                fence.compensation_id,
                fence.compensation_generation,
                review_uid,
                &resolution,
                now + Duration::milliseconds(6),
            )
            .await?,
        CompensationReviewResolutionOutcome::Replayed(_)
    ));
    Ok(())
}

#[tokio::test]
async fn direct_external_callback_waits_for_compensation_hand_release_db() -> TestResult {
    // Pins: a terminal provider callback received during Cancelling is persisted without settling
    // the compensation; the verified hand-release finalizer then consumes it exactly once.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["direct-external"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let release_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::ExternalJob,
    );
    let external_job_uid = Uuid::now_v7();
    let provider_job_id = format!("provider-job-{}", Uuid::now_v7());
    let idempotency_key = format!("idempotency-{}", Uuid::now_v7());
    let owner = ExecutionExternalJobOwner::Compensation {
        compensation_id: fence.compensation_id.as_uuid(),
        compensation_generation: fence.compensation_generation,
        compensation_attempt_generation: fence.attempt_generation,
    };
    repository
        .reserve_external_job_intent(
            scope,
            &config,
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                provider: "direct-provider".to_string(),
                idempotency_key: idempotency_key.clone(),
                expires_at: now + Duration::minutes(1),
            },
        )
        .await?;
    repository
        .bind_external_job(
            scope,
            &config,
            ExecutionExternalJobBinding {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                idempotency_key,
                provider: "direct-provider".to_string(),
                provider_job_id: provider_job_id.clone(),
                callback_auth_reference: "vault://direct-callback".to_string(),
                state: ExecutionExternalJobState::Running,
                progress_phase: Some("running".to_string()),
                cancel_supported: true,
                provider_contract_violation: None,
                next_reconcile_at: Some(now + Duration::minutes(2)),
            },
        )
        .await?;
    let started = repository
        .begin_compensation_external_release(
            &release_request,
            external_job_uid,
            now + Duration::milliseconds(2),
        )
        .await?;
    assert!(matches!(
        started,
        moa_execution::repository::compensation::CompensationAttemptExternalOutcome::Applied {
            ref attempt,
            ..
        } if attempt.attempt_state == CompensationAttemptState::Cancelling
            && attempt.release_intent == Some(ExecutionCompensationReleaseIntent::ExternalJob)
    ));
    let callback = repository
        .apply_external_job_callback_and_activate(
            scope,
            &config,
            ExecutionExternalJobCallback {
                external_job_uid,
                job_generation: 1,
                provider: "direct-provider".to_string(),
                provider_job_id,
                provider_event_id: format!("event-{}", Uuid::now_v7()),
                update: ExecutionExternalJobCallbackUpdate::Terminal {
                    state: ExecutionExternalJobState::Completed,
                    progress_phase: Some("completed".to_string()),
                    output: Some(json!({"undone": true})),
                    error: None,
                },
            },
        )
        .await?;
    assert!(matches!(
        callback.outcome,
        ExecutionExternalJobCallbackOutcome::Applied(ref job)
            if job.state == ExecutionExternalJobState::Completed
    ));
    assert_eq!(callback.activation, None);
    let waiting_before_release: String = sqlx::query_scalar(
        "SELECT attempt_state FROM moa.execution_compensation \
         WHERE run_uid=$1 AND compensation_id=$2",
    )
    .bind(run.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(waiting_before_release, "cancelling");
    let release_receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &release_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    let finalized = repository
        .yield_released_compensation_attempt_to_external_job(
            &release_request,
            external_job_uid,
            Some(release_receipt),
            now + Duration::milliseconds(3),
        )
        .await?;
    assert!(matches!(
        finalized,
        moa_execution::repository::compensation::CompensationAttemptExternalOutcome::Applied {
            ref attempt,
            ..
        } if attempt.attempt_state == CompensationAttemptState::Terminal
    ));
    Ok(())
}

#[tokio::test]
async fn terminal_compensation_failure_finalizes_manual_repair_with_nested_cause_db() -> TestResult
{
    // Pins: a non-retryable compensation failure terminalizes the run as CompensationFailed and
    // replaces the held terminal cause with CompensationFailure carrying the original terminal
    // intent, the exact compensation identity, and the verbatim undo outcome, under manual repair.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let undo_failure = ExecutionCompensationOutcome::Failed {
        message: "undo was rejected permanently".to_string(),
        retryable: false,
        usage: usage(1),
    };
    let (compensation_id, current) = settled_compensation_run(
        &repository,
        test_db.store().pool(),
        scope,
        tenant_id,
        "undoable",
        undo_failure.clone(),
    )
    .await?;

    let PendingTerminalAdvanceOutcome::Applied(commit) = repository
        .advance_pending_terminal_settlement(
            &config,
            scope,
            current.run_uid,
            current.controller_generation,
            current.wake_epoch,
            moa_test_support::fixtures::pg_now(),
            1,
        )
        .await?
    else {
        panic!("a failed compensation must advance its held terminal to manual repair");
    };
    assert_eq!(
        commit.stage,
        PendingTerminalAdvanceStage::ManualRepairRequired
    );
    assert!(!commit.work_remaining);
    assert!(commit.continuation.is_none());
    assert!(commit.compensation_admission.is_none());

    let expected_cause = ExecutionTerminalCause::CompensationFailure {
        original_status: ExecutionRunStatus::Failed,
        original_reason: ExecutionTerminalReason::InternalFailure,
        original_cause: Box::new(ExecutionTerminalCause::InternalFailure),
        compensation_id,
        outcome: undo_failure,
    };
    for observed in [
        commit.run.clone(),
        repository
            .load_run(scope, current.run_uid)
            .await?
            .expect("manual-repair run stays visible after finalization"),
    ] {
        assert_eq!(observed.status, ExecutionRunStatus::Failed);
        assert_eq!(
            observed.terminal_reason,
            Some(ExecutionTerminalReason::CompensationFailed)
        );
        assert!(observed.manual_repair_required);
        assert!(observed.pending_terminal.is_none());
        assert_eq!(
            observed
                .terminal_evidence
                .as_ref()
                .map(|evidence| &evidence.cause),
            Some(&expected_cause)
        );
    }
    assert_eq!(
        held_non_lifetime_capacity(test_db.store().pool(), current.run_uid).await?,
        0,
        "a manual-repair terminal must leave no non-lifetime capacity receipt held"
    );
    Ok(())
}

#[tokio::test]
async fn successful_compensation_finalizes_the_original_held_terminal_db() -> TestResult {
    // Pins: when every undo succeeds, the bounded drain installs the terminal intent the run was
    // already holding — unchanged and without manual repair — instead of tripping the
    // "completed compensation retained non-lifetime capacity" guard. Both terminal-drain branches
    // probe for held reservations, so a receipt leaked by compensation settlement wedges the
    // success path exactly like the failure path.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let (_, current) = settled_compensation_run(
        &repository,
        test_db.store().pool(),
        scope,
        tenant_id,
        "undone",
        ExecutionCompensationOutcome::Completed {
            output: json!({"tokens": 0}),
            usage: usage(1),
        },
    )
    .await?;

    let PendingTerminalAdvanceOutcome::Applied(commit) = repository
        .advance_pending_terminal_settlement(
            &config,
            scope,
            current.run_uid,
            current.controller_generation,
            current.wake_epoch,
            moa_test_support::fixtures::pg_now(),
            1,
        )
        .await?
    else {
        panic!("a fully compensated run must advance its held terminal to finalization");
    };
    assert_eq!(commit.stage, PendingTerminalAdvanceStage::Finalized);
    assert!(!commit.work_remaining);
    assert!(commit.continuation.is_none());
    assert!(commit.compensation_admission.is_none());

    for observed in [
        commit.run.clone(),
        repository
            .load_run(scope, current.run_uid)
            .await?
            .expect("finalized run stays visible after successful compensation"),
    ] {
        assert_eq!(observed.status, ExecutionRunStatus::Failed);
        assert_eq!(
            observed.terminal_reason,
            Some(ExecutionTerminalReason::InternalFailure),
            "a successful undo must not rewrite the reason the run was failing for"
        );
        assert!(
            !observed.manual_repair_required,
            "a successful undo needs no operator repair"
        );
        assert!(observed.pending_terminal.is_none());
        assert_eq!(
            observed
                .terminal_evidence
                .as_ref()
                .map(|evidence| &evidence.cause),
            Some(&ExecutionTerminalCause::InternalFailure),
            "the held cause must survive compensation verbatim"
        );
    }
    assert_eq!(
        held_non_lifetime_capacity(test_db.store().pool(), current.run_uid).await?,
        0,
        "a finalized terminal must leave no non-lifetime capacity receipt held"
    );
    Ok(())
}

/// Drives a compensating run's single reverse-order slice to one exact terminal undo outcome.
///
/// Returns the settled compensation identity and the run reloaded afterwards, which is the state
/// the next controller activation observes before it advances the run's held terminal intent.
async fn settled_compensation_run(
    repository: &ExecutionRepository,
    pool: &sqlx::PgPool,
    scope: ExecutionScope,
    tenant_id: TenantId,
    node_id: &str,
    outcome: ExecutionCompensationOutcome,
) -> Result<
    (moa_execution::state::CompensationId, ExecutionRunRecord),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let expected_status = match outcome {
        ExecutionCompensationOutcome::Completed { .. } => CompensationStatus::Completed,
        ExecutionCompensationOutcome::Failed { .. } => CompensationStatus::Failed,
        ExecutionCompensationOutcome::UnknownOutcome { .. } => CompensationStatus::UnknownOutcome,
    };
    let (run, _) = compensating_run(repository, scope, tenant_id, &[node_id]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(repository, scope, &config, run.run_uid, now).await?;
    let compensation_id = admission.attempt.registration.compensation_id;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Outcome,
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&request, now + Duration::milliseconds(2))
            .await?,
        CompensationAttemptReleaseClaimOutcome::Applied(_)
    ));
    let release_receipt =
        persist_compensation_release_receipt(pool, &request, now + Duration::milliseconds(3))
            .await?;
    let CompensationAttemptWriteOutcome::Applied(settled) = repository
        .settle_released_compensation_attempt(
            &request,
            outcome,
            now + Duration::milliseconds(3),
            Some(release_receipt),
        )
        .await?
    else {
        panic!("a verified release must settle the compensation attempt");
    };
    assert_eq!(settled.registration.status, expected_status);
    assert_eq!(settled.attempt_state, CompensationAttemptState::Terminal);
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("settled-compensation run stays visible before finalization");
    assert_eq!(current.status, ExecutionRunStatus::Compensating);
    Ok((compensation_id, current))
}

/// Counts capacity receipts a terminal run must never still hold.
///
/// `active_runs` and `parked_runs` are lifetime receipts released by finalization itself; the
/// other three dimensions are what both terminal-drain branches refuse to finalize against.
async fn held_non_lifetime_capacity(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation WHERE run_uid=$1 \
         AND resource_dimension IN ('active_tasks','scheduled_triggers','external_jobs') \
         AND state IN ('reserved','reconciling')",
    )
    .bind(run_uid)
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn compensation_cancel_releases_capacity_only_after_verified_finalize_db() -> TestResult {
    // Pins: claiming compensation teardown makes the attempt non-dispatchable but preserves its
    // active capacity until the exact cancellation receiver reports provider release.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (run, _) = compensating_run(&repository, scope, tenant_id, &["cancelled"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let config = ExecutionConfig::default();
    let admission =
        active_compensation_admission(&repository, scope, &config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::RunTerminal,
    );
    let CompensationAttemptReleaseClaimOutcome::Applied(claimed) = repository
        .begin_compensation_attempt_release(&request, now + Duration::milliseconds(2))
        .await?
    else {
        panic!("exact cancellation must claim the active compensation");
    };
    assert_eq!(claimed.attempt_state, CompensationAttemptState::Cancelling);
    assert_eq!(
        claimed.release_intent,
        Some(ExecutionCompensationReleaseIntent::RunTerminal)
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&request, now + Duration::milliseconds(2))
            .await?,
        CompensationAttemptReleaseClaimOutcome::Replayed(_)
    ));
    let mut conflicting_intent = request.clone();
    conflicting_intent.intent = ExecutionCompensationReleaseIntent::Outcome;
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(
                &conflicting_intent,
                now + Duration::milliseconds(2),
            )
            .await?,
        CompensationAttemptReleaseClaimOutcome::Stale
    ));
    let reservation_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation WHERE reservation_uid=$1",
    )
    .bind(admission.capacity_reservation_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(reservation_state, "reserved");

    assert!(matches!(
        repository
            .settle_released_compensation_attempt(
                &request,
                ExecutionCompensationOutcome::Failed {
                    message: "run terminal fence".to_string(),
                    retryable: false,
                    usage: usage(0),
                },
                now + Duration::milliseconds(3),
                None,
            )
            .await?,
        CompensationAttemptWriteOutcome::Conflict
    ));
    let release_receipt = persist_compensation_release_receipt(
        test_db.store().pool(),
        &request,
        now + Duration::milliseconds(3),
    )
    .await?;

    let CompensationAttemptWriteOutcome::Applied(settled) = repository
        .settle_released_compensation_attempt(
            &request,
            ExecutionCompensationOutcome::Failed {
                message: "run terminal fence".to_string(),
                retryable: false,
                usage: usage(0),
            },
            now + Duration::milliseconds(3),
            Some(release_receipt),
        )
        .await?
    else {
        panic!("verified cancellation must settle the compensation");
    };
    assert_eq!(settled.attempt_state, CompensationAttemptState::Terminal);
    let reservation_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation WHERE reservation_uid=$1",
    )
    .bind(admission.capacity_reservation_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(reservation_state, "released");
    Ok(())
}

async fn persist_compensation_release_receipt(
    pool: &sqlx::PgPool,
    request: &ExecutionCompensationAttemptCancelRequest,
    released_at: chrono::DateTime<chrono::Utc>,
) -> Result<moa_core::types::sandbox_workspace::ExecutionHandReleaseReceipt, sqlx::Error> {
    use moa_core::types::{
        identifiers::{ExecutionCompensationScopeId, ExecutionRunScopeId},
        sandbox_workspace::{ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt},
    };

    let receipt = ExecutionHandReleaseReceipt {
        receipt_id: Uuid::now_v7(),
        tenant_id: request.tenant_id,
        run_id: ExecutionRunScopeId(request.run_uid),
        owner: ExecutionHandReleaseOwner::Compensation {
            compensation_id: ExecutionCompensationScopeId(request.compensation_id.as_uuid()),
            logical_generation: request.compensation_generation,
        },
        attempt_generation: request.compensation_attempt_generation,
        workspace_id: None,
        writer_epoch: None,
        instance_generation: None,
        hand_provisioning_operation_id: None,
        hand_lease_generation: None,
        checkpoint_id: None,
        checkpoint_generation: None,
        checkpoint_manifest_digest: None,
        checkpoint_logical_bytes: None,
        requested_at: released_at,
        released_at,
    };
    sqlx::query(
        "INSERT INTO moa.sandbox_execution_hand_release_receipts \
         (receipt_id,tenant_id,run_uid,owner_kind,task_id,compensation_id, \
          logical_generation,attempt_generation,workspace_id,writer_epoch,instance_generation, \
          hand_provisioning_operation_id,hand_lease_generation,checkpoint_id, \
          checkpoint_generation,checkpoint_manifest_digest,checkpoint_logical_bytes, \
          receipt_state,destroy_outcome,claim_token,claim_expires_at,requested_at,deadline_at, \
          released_at) VALUES ($1,$2,$3,'compensation',NULL,$4,$5,$6,NULL,NULL,NULL,NULL,NULL, \
          NULL,NULL,NULL,NULL,'released','verified_absent',NULL,NULL,$7,$7,$7)",
    )
    .bind(receipt.receipt_id)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(i64::try_from(request.compensation_generation).expect("fixture generation fits i64"))
    .bind(
        i64::try_from(request.compensation_attempt_generation)
            .expect("fixture attempt generation fits i64"),
    )
    .bind(released_at)
    .execute(pool)
    .await?;
    Ok(receipt)
}

async fn park_compensation_review_then_pause(
    repository: &ExecutionRepository,
    pool: &sqlx::PgPool,
    scope: ExecutionScope,
    tenant_id: TenantId,
    config: &ExecutionConfig,
) -> Result<
    (ExecutionRunRecord, CompensationAttemptFence, Uuid),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (run, _) = compensating_run(repository, scope, tenant_id, &["paused-review"]).await?;
    let now = moa_test_support::fixtures::pg_now();
    let admission =
        active_compensation_admission(repository, scope, config, run.run_uid, now).await?;
    let fence = fence(&admission);
    assert!(matches!(
        repository
            .start_compensation_attempt(scope, fence, now + Duration::milliseconds(1))
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let review_uid = Uuid::now_v7();
    let release_request = cancel_request(
        &admission,
        tenant_id,
        ExecutionCompensationReleaseIntent::Review,
    );
    assert!(matches!(
        repository
            .begin_compensation_attempt_release(&release_request, now + Duration::milliseconds(2),)
            .await?,
        CompensationAttemptReleaseClaimOutcome::Applied(_)
    ));
    let release_receipt = persist_compensation_release_receipt(
        pool,
        &release_request,
        now + Duration::milliseconds(3),
    )
    .await?;
    assert!(matches!(
        repository
            .park_released_compensation_review(
                &release_request,
                review_uid,
                now + Duration::minutes(5),
                now + Duration::milliseconds(3),
                Some(release_receipt),
            )
            .await?,
        CompensationAttemptWriteOutcome::Applied(_)
    ));
    let TransitionOutcome::RunApplied(paused) = repository
        .pause_run(scope, config, run.run_uid, run.controller_generation)
        .await?
    else {
        panic!("a storage-only compensation review must permit an exact run pause");
    };
    assert_eq!(paused.status, ExecutionRunStatus::Paused);
    assert_eq!(
        paused.controller_generation,
        run.controller_generation + 1,
        "pause must advance only the run controller generation"
    );
    Ok((paused, fence, review_uid))
}

async fn run_activation_count(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    controller_generation: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox WHERE run_uid=$1 \
         AND controller_generation=$2 AND dispatch_kind='run_activation' \
         AND state IN ('pending','dispatching')",
    )
    .bind(run_uid)
    .bind(i64::try_from(controller_generation).expect("fixture controller generation fits i64"))
    .fetch_one(pool)
    .await
}

fn fence(
    admission: &moa_execution::repository::compensation::CompensationAttemptAdmission,
) -> CompensationAttemptFence {
    CompensationAttemptFence {
        run_uid: admission.attempt.registration.run_uid,
        compensation_id: admission.attempt.registration.compensation_id,
        controller_generation: admission.attempt.controller_generation,
        compensation_generation: admission.attempt.registration.generation,
        attempt_generation: admission.attempt.attempt_generation,
        dispatch_uid: admission.dispatch.dispatch_uid,
    }
}

fn cancel_request(
    admission: &moa_execution::repository::compensation::CompensationAttemptAdmission,
    tenant_id: TenantId,
    intent: ExecutionCompensationReleaseIntent,
) -> ExecutionCompensationAttemptCancelRequest {
    let fence = fence(admission);
    ExecutionCompensationAttemptCancelRequest {
        cancellation_dispatch_uid: Uuid::now_v7(),
        tenant_id,
        run_uid: fence.run_uid,
        compensation_id: fence.compensation_id,
        controller_generation: fence.controller_generation,
        attempt_controller_generation: fence.controller_generation,
        compensation_generation: fence.compensation_generation,
        compensation_attempt_generation: fence.attempt_generation,
        active_dispatch_uid: fence.dispatch_uid,
        capacity_reservation_uid: admission.capacity_reservation_uid,
        watchdog_trigger_uid: admission.watchdog.trigger.trigger_uid,
        intent,
    }
}

async fn active_compensation_admission(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &ExecutionConfig,
    run_uid: Uuid,
    now: chrono::DateTime<Utc>,
) -> Result<CompensationAttemptAdmission, moa_execution::Error> {
    match repository
        .admit_next_compensation_attempt(scope, config, run_uid, now)
        .await?
    {
        CompensationAttemptAdmissionOutcome::Admitted(admission)
        | CompensationAttemptAdmissionOutcome::Replayed(admission) => Ok(*admission),
        other => panic!("compensation fixture must own one active slice, got {other:?}"),
    }
}

fn explain_index_scan<'a>(
    value: &'a serde_json::Value,
    expected_index: &str,
) -> Option<&'a serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("Index Name").and_then(serde_json::Value::as_str) == Some(expected_index)
            {
                return Some(object);
            }
            object
                .values()
                .find_map(|child| explain_index_scan(child, expected_index))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .find_map(|child| explain_index_scan(child, expected_index)),
        _ => None,
    }
}

async fn compensating_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    tenant_id: TenantId,
    node_ids: &[&str],
) -> Result<
    (
        moa_execution::repository::ExecutionRunRecord,
        Vec<moa_execution::state::CompensationRegistrationProjection>,
    ),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        &format!("compensation-attempt-{}", Uuid::now_v7()),
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(repository, scope, new).await?;
    let tasks = node_ids
        .iter()
        .map(|node_id| {
            compensated_task(
                run.run_uid,
                node_id,
                forward_reference.clone(),
                compensation.clone(),
            )
        })
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
        .await?;
    for task in &tasks {
        reserve_and_start(repository, scope, run.run_uid, task.task_id).await?;
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
    }
    let run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("fixture run exists");
    let config = ExecutionConfig::default();
    let PendingTerminalAdvanceOutcome::Applied(commit) = repository
        .fence_completion_terminal_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Failed,
                reason: ExecutionTerminalReason::InternalFailure,
                terminal_evidence: ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::InternalFailure,
                    satisfied_requirement_count: 0,
                    requirement_count: 0,
                },
                completion_check_results: Vec::new(),
                terminal_gaps: vec!["forward failure".to_string()],
                output: None,
                cancellation_reason: None,
            },
            moa_test_support::fixtures::pg_now(),
            1,
        )
        .await?
    else {
        panic!("fixture bounded terminal page must apply");
    };
    let admission = if let Some(admission) = commit.compensation_admission {
        admission
    } else {
        assert_eq!(commit.run.status, ExecutionRunStatus::Compensating);
        let PendingTerminalAdvanceOutcome::Applied(continuation) = repository
            .advance_pending_terminal_settlement(
                &config,
                scope,
                commit.run.run_uid,
                commit.run.controller_generation,
                commit.run.wake_epoch,
                moa_test_support::fixtures::pg_now(),
                1,
            )
            .await?
        else {
            panic!("fixture compensation continuation must apply");
        };
        continuation
            .compensation_admission
            .expect("compensation continuation must admit one reverse-order slice")
    };
    Ok((commit.run, vec![admission.attempt.registration.clone()]))
}

fn compensated_catalog() -> (
    ExecutionCapabilityCatalog,
    CapabilityReference,
    ExecutionCompensation,
) {
    let mut forward = capability("effects.commit");
    let compensator = capability("effects.undo");
    let compensation = ExecutionCompensation {
        compensator: compensator.reference.clone(),
        input_mapping: token_mapping(),
    };
    forward.rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    let forward_reference = forward.reference.clone();
    let catalog = ExecutionCapabilityCatalog::build(vec![forward, compensator])
        .expect("compensated test catalog must be valid");
    (catalog, forward_reference, compensation)
}

fn capability(name: &str) -> ExecutionCapability {
    let source = CapabilitySource::BuiltInTool {
        name: name.to_string(),
    };
    ExecutionCapability {
        reference: CapabilityReference {
            name: name.to_string(),
            version: "v1".to_string(),
        },
        contract_revision: "contract-v1".to_string(),
        description: format!("test capability {name}"),
        input_schema: json!({"type":"object","required":["tokens"],"properties":{"tokens":{"type":"integer"}},"additionalProperties":false}),
        output_schema: json!({"type":"object","required":["tokens"],"properties":{"tokens":{"type":"integer"}},"additionalProperties":false}),
        action_class: ActionClass::ExternalWrite,
        risk_level: RiskLevel::Medium,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        async_mode: moa_core::types::tools::ToolAsyncMode::SynchronousOnly,
        execution_class: ExecutionClass::External,
        requires_sandbox: false,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: estimate(1),
        rollback: None,
    }
}

fn token_mapping() -> CompensationInputMapping {
    CompensationInputMapping {
        bindings: vec![CompensationInputBinding {
            target_pointer: "/tokens".to_string(),
            source: CompensationValueSource::OriginalOutput {
                pointer: "/tokens".to_string(),
            },
        }],
    }
}

fn compensated_task(
    run_uid: Uuid,
    node_id: &str,
    forward_reference: CapabilityReference,
    compensation: ExecutionCompensation,
) -> LogicalTask {
    let mut task = logical_task(run_uid, node_id, "", estimate(1));
    task.input = json!({"tokens": 1});
    task.kind = LogicalTaskKind::Capability {
        reference: forward_reference,
    };
    task.compensation = Some(compensation);
    task
}
