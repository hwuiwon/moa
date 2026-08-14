//! Shared task transition evidence.

use super::outbox::{ExecutionDispatchKind, NewExecutionDispatch, enqueue_dispatch_in_conn};
use super::outcome::record_task_outcome_in_conn;
use super::ready::transition_node_counters_in_tx;
use super::rows::{required_u64, run_from_row};
use super::run::enqueue_run_activation_in_conn;
use super::sql::{LOAD_RUN_FOR_UPDATE_SQL, LOAD_TASK_FOR_UPDATE_SQL};
use super::trigger::{
    ExecutionTriggerKind, ExecutionTriggerNoOp, ExecutionWaitTriggerDeliveryOutcome,
    deliver_wait_trigger_in_conn,
};
use super::*;
use crate::state::{WaitSettlement, completed_task_outcome, failed_task_outcome};
use crate::wire::{
    ExecutionAttemptCancelReason, ExecutionCompensationAttemptCancelRequest,
    ExecutionTaskAttemptCancelRequest,
};
use moa_artifacts::execution_plan::{ExecutionFailureClass, ExecutionWaitExpiryAction};
use moa_config::ExecutionConfig;

use super::capacity::{
    CapacityReleaseOutcome, CapacityReserveOutcome, ExecutionCapacityDimension,
    parked_run_capacity_request, prelock_capacity_dimensions_in_tx,
    release_parked_run_capacity_in_tx, reserve_capacity_in_tx, transfer_active_run_to_parked_in_tx,
    transfer_parked_run_to_active_in_tx,
};

const PAUSE_CANCEL_NAMESPACE: Uuid = Uuid::from_u128(0x7c90_7811_f496_5ace_a244_b645_8cc1_0a73);

impl ExecutionRepository {
    /// Fences one current run from new reservations and drains its active bounded attempts.
    pub async fn pause_run(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        expected_controller_generation: u64,
    ) -> Result<TransitionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_run WHERE run_uid=$1",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
            ],
        )
        .await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        if run.controller_generation != expected_controller_generation {
            let replay_generation = expected_controller_generation.checked_add(1);
            let replayed = replay_generation == Some(run.controller_generation)
                && matches!(
                    run.status,
                    ExecutionRunStatus::PauseRequested
                        | ExecutionRunStatus::Pausing
                        | ExecutionRunStatus::Paused
                );
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replayed {
                TransitionOutcome::RunAlreadyApplied(run)
            } else {
                TransitionOutcome::Rejected(TransitionRejection::GenerationMismatch)
            });
        }
        if matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::RunAlreadyApplied(run));
        }
        if run.status.is_terminal() || run.status == ExecutionRunStatus::AwaitingConfirmation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }

        let next_generation = run.controller_generation.checked_add(1).ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "execution controller generation overflow".to_string(),
            }
        })?;
        let row = sqlx::query(
            "UPDATE moa.execution_run SET status='pause_requested', \
             controller_generation=$2, activation_state='paused', \
             pause_requested_at=COALESCE(pause_requested_at, clock_timestamp()), \
             last_progress_at=GREATEST(last_progress_at, clock_timestamp()), \
             updated_at=NOW() WHERE run_uid=$1 RETURNING *",
        )
        .bind(run_uid)
        .bind(to_i64(next_generation, "controller generation")?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let pause_fence = run_from_row(&row)?;
        enqueue_pause_cancellations(
            conn.as_mut(),
            &pause_fence,
            run.controller_generation,
            config.max_fleet_active_tasks,
        )
        .await?;
        let target_status = if run.active_task_count == 0 {
            "paused"
        } else {
            "pausing"
        };
        let row = sqlx::query(
            "UPDATE moa.execution_run SET status=$2, \
             paused_at=CASE WHEN $2='paused' THEN clock_timestamp() ELSE NULL END, \
             updated_at=NOW() WHERE run_uid=$1 RETURNING *",
        )
        .bind(run_uid)
        .bind(target_status)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let updated = run_from_row(&row)?;
        let released_parked = match release_parked_run_capacity_in_tx(
            conn.as_mut(),
            run.tenant_id,
            run.run_uid,
            run.controller_generation,
        )
        .await?
        {
            CapacityReleaseOutcome::Released => true,
            CapacityReleaseOutcome::AlreadyReleased | CapacityReleaseOutcome::NotFound => false,
            CapacityReleaseOutcome::Stale => {
                return Err(Error::InvalidRepositoryData {
                    message: "pause encountered a stale parked-run capacity receipt".to_string(),
                });
            }
        };
        let parked = if released_parked {
            reserve_capacity_in_tx(
                conn.as_mut(),
                config,
                parked_run_capacity_request(&updated, updated.wake_epoch),
            )
            .await?
        } else {
            transfer_active_run_to_parked_in_tx(conn.as_mut(), config, &updated, updated.wake_epoch)
                .await?
        };
        if parked == CapacityReserveOutcome::Saturated {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::CapacitySaturated {
                dimension: ExecutionCapacityDimension::ParkedRuns.as_str(),
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::RunApplied(updated))
    }

    /// Resumes one fully drained paused run and enqueues exactly one new controller activation.
    pub async fn resume_run(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        expected_controller_generation: u64,
    ) -> Result<TransitionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_run WHERE run_uid=$1",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
            ],
        )
        .await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        if run.controller_generation != expected_controller_generation {
            let replay_generation = expected_controller_generation.checked_add(1);
            let replayed = replay_generation == Some(run.controller_generation)
                && run.pause_requested_at.is_some()
                && matches!(
                    run.status,
                    ExecutionRunStatus::Queued
                        | ExecutionRunStatus::Running
                        | ExecutionRunStatus::Compensating
                );
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replayed {
                TransitionOutcome::RunAlreadyApplied(run)
            } else {
                TransitionOutcome::Rejected(TransitionRejection::GenerationMismatch)
            });
        }
        if run.status != ExecutionRunStatus::Paused
            || run.active_task_count != 0
            || run.activation_state != ExecutionActivationState::Paused
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        let owns_active_capacity: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
             WHERE run_uid=$1 AND state IN ('reserved','reconciling') \
               AND resource_dimension='active_tasks')",
        )
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if owns_active_capacity {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        if transfer_parked_run_to_active_in_tx(
            conn.as_mut(),
            run.tenant_id,
            run.run_uid,
            run.controller_generation,
        )
        .await?
            == CapacityReserveOutcome::Saturated
        {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::CapacitySaturated {
                dimension: ExecutionCapacityDimension::ActiveRuns.as_str(),
            });
        }
        let next_generation = run.controller_generation.checked_add(1).ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "execution controller generation overflow".to_string(),
            }
        })?;
        sqlx::query(
            "UPDATE moa.execution_run SET \
             status=CASE WHEN pending_terminal_status IS NULL THEN 'queued' \
                         ELSE 'compensating' END, controller_generation=$2, \
             activation_state='idle', paused_at=NULL, \
             last_progress_at=GREATEST(last_progress_at, clock_timestamp()), updated_at=NOW() \
             WHERE run_uid=$1",
        )
        .bind(run_uid)
        .bind(to_i64(next_generation, "controller generation")?)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            run.tenant_id,
            run_uid,
            next_generation,
            Utc::now(),
            json!({"reason":"run_resumed"}),
        )
        .await?;
        let row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let updated = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::RunApplied(updated))
    }

    /// Delivers and settles one exact due task wait, activating only a non-paused run.
    pub async fn fire_wait_trigger(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        trigger_uid: Uuid,
    ) -> Result<(TransitionOutcome, Option<ExecutionDispatchRecord>)> {
        let mut conn = scope.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_trigger WHERE trigger_uid=$1",
        )
        .bind(trigger_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok((TransitionOutcome::NotFound, None));
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
                ExecutionCapacityDimension::ScheduledTriggers,
            ],
        )
        .await?;
        let delivery = deliver_wait_trigger_in_conn(conn.as_mut(), trigger_uid).await?;
        let (trigger, observed_at) = match delivery {
            ExecutionWaitTriggerDeliveryOutcome::Delivered {
                trigger,
                observed_at,
            } => (trigger, observed_at),
            ExecutionWaitTriggerDeliveryOutcome::NoOp(ExecutionTriggerNoOp::NotFound) => {
                conn.commit().await.map_err(storage_error)?;
                return Ok((TransitionOutcome::NotFound, None));
            }
            ExecutionWaitTriggerDeliveryOutcome::NoOp(ExecutionTriggerNoOp::Duplicate) => {
                let replay_row = sqlx::query(
                    "SELECT task.* FROM moa.execution_trigger AS trigger \
                     JOIN moa.execution_task AS task \
                       ON task.run_uid=trigger.run_uid AND task.task_id=trigger.task_id \
                      AND task.tenant_id=trigger.tenant_id \
                     WHERE trigger.trigger_uid=$1 FOR UPDATE OF task",
                )
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                let outcome = match replay_row {
                    Some(row) => {
                        let task = super::rows::task_from_row(&row)?;
                        if task.status.is_terminal()
                            && task.generation_history.iter().any(|entry| {
                                entry.get("kind").and_then(Value::as_str)
                                    == Some("storage_wait_settlement")
                            })
                        {
                            TransitionOutcome::AlreadyApplied(task)
                        } else {
                            TransitionOutcome::Rejected(TransitionRejection::InvalidTaskStatus)
                        }
                    }
                    None => TransitionOutcome::NotFound,
                };
                conn.commit().await.map_err(storage_error)?;
                return Ok((outcome, None));
            }
            ExecutionWaitTriggerDeliveryOutcome::NoOp(
                ExecutionTriggerNoOp::Inactive
                | ExecutionTriggerNoOp::StaleGeneration
                | ExecutionTriggerNoOp::NotDue,
            ) => {
                conn.commit().await.map_err(storage_error)?;
                return Ok((
                    TransitionOutcome::Rejected(TransitionRejection::InvalidTaskStatus),
                    None,
                ));
            }
        };
        let run_uid = trigger
            .run_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "wait trigger is missing run identity".to_string(),
            })?;
        let task_id = ExecutionTaskId::from_uuid(trigger.task_id.ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "wait trigger is missing task identity".to_string(),
            }
        })?);
        let run_row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;
        let task_row = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = super::rows::task_from_row(&task_row)?;
        let waiting_since = task
            .waiting_since
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "current wait trigger task has no wait-entry timestamp".to_string(),
            })?;
        let settlement = settlement_for_delivered_trigger(&task, trigger.kind)?;
        let outcome = settle_wait_locked_in_conn(
            &mut conn,
            &run,
            &task,
            task.generation,
            waiting_since,
            settlement,
            observed_at,
        )
        .await?;
        if matches!(outcome, TransitionOutcome::Rejected(_)) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok((outcome, None));
        }
        let current_run_row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let current_run = run_from_row(&current_run_row)?;
        let activation = if matches!(outcome, TransitionOutcome::Applied(_))
            && !matches!(
                current_run.status,
                ExecutionRunStatus::PauseRequested
                    | ExecutionRunStatus::Pausing
                    | ExecutionRunStatus::Paused
            ) {
            Some(
                enqueue_run_activation_in_conn(
                    conn.as_mut(),
                    current_run.tenant_id,
                    run_uid,
                    current_run.controller_generation,
                    observed_at,
                    json!({ "trigger_uid": trigger_uid, "reason": "storage_wait_settled" }),
                )
                .await?,
            )
        } else {
            None
        };
        conn.commit().await.map_err(storage_error)?;
        Ok((outcome, activation))
    }
}

async fn settle_wait_locked_in_conn(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    expected_task_generation: u64,
    expected_waiting_since: DateTime<Utc>,
    settlement: WaitSettlement,
    settled_at: DateTime<Utc>,
) -> Result<TransitionOutcome> {
    if wait_settlement_is_exact_replay(
        task,
        expected_task_generation,
        expected_waiting_since,
        &settlement,
    ) {
        return Ok(TransitionOutcome::AlreadyApplied(task.clone()));
    }
    if task.generation != expected_task_generation {
        return Ok(TransitionOutcome::Rejected(
            TransitionRejection::GenerationMismatch,
        ));
    }
    let outcome = wait_settlement_outcome(task, &settlement)?;
    if run.status.is_terminal() {
        return Ok(TransitionOutcome::Rejected(
            TransitionRejection::InvalidRunStatus,
        ));
    }
    if task.waiting_since != Some(expected_waiting_since)
        || task.attempt_state != ExecutionAttemptState::Waiting
    {
        return Ok(TransitionOutcome::Rejected(
            TransitionRejection::InvalidTaskStatus,
        ));
    }
    let Some(run_deadline_at) = run.approved_budget.deadline_at else {
        return Err(Error::InvalidRepositoryData {
            message: "storage-only wait run has no absolute deadline".to_string(),
        });
    };
    if settled_at >= run_deadline_at {
        return Ok(TransitionOutcome::Rejected(
            TransitionRejection::DeadlineElapsed,
        ));
    }
    let due_at =
        wait_settlement_due_at(task, &settlement, expected_waiting_since, run_deadline_at)?;
    if settled_at < due_at {
        return Ok(TransitionOutcome::Rejected(
            TransitionRejection::InvalidTaskStatus,
        ));
    }

    let waiting_status = task.status;
    let input_audience = if waiting_status == ExecutionTaskStatus::WaitingInput {
        Some(
            task.current_outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience.clone()),
                    _ => None,
                })
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "waiting-input task is missing its typed input audience".to_string(),
                })?,
        )
    } else {
        None
    };
    let transitioned = sqlx::query(
        "UPDATE moa.execution_task SET status='running', attempt_state='running', \
             generation_history = generation_history || jsonb_build_array($5::JSONB), \
             last_progress_at=GREATEST(last_progress_at, $6), updated_at=NOW() \
         WHERE run_uid=$1 AND task_id=$2 AND generation=$3 \
           AND waiting_since=$4 AND attempt_state='waiting'",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(expected_task_generation, "wait task generation")?)
    .bind(expected_waiting_since)
    .bind(json!({
        "kind": "storage_wait_settlement",
        "controller_generation_at_settlement": run.controller_generation,
        "task_generation": expected_task_generation,
        "waiting_since": expected_waiting_since,
        "settled_at": settled_at,
        "settlement": settlement,
    }))
    .bind(settled_at)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if transitioned.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "current storage wait lost its exact settlement fence".to_string(),
        });
    }
    let write = record_task_outcome_in_conn(
        conn,
        run.run_uid,
        task.task_id,
        expected_task_generation,
        outcome,
    )
    .await?;
    let settled_task = match write {
        TaskOutcomeWrite::Applied { task, .. } | TaskOutcomeWrite::Replayed { task, .. } => task,
        TaskOutcomeWrite::NotFound => {
            return Err(Error::InvalidRepositoryData {
                message: "storage wait task disappeared during settlement".to_string(),
            });
        }
        TaskOutcomeWrite::Rejected { reason, .. } => {
            return Err(Error::InvalidRepositoryData {
                message: format!("canonical storage wait outcome was rejected: {reason:?}"),
            });
        }
    };
    if let Some(input_audience) = input_audience.as_ref() {
        super::ready::transition_node_counters_with_input_audience_in_tx(
            conn,
            run.run_uid,
            &settled_task.node_id,
            &settled_task.item_key,
            waiting_status,
            settled_task.status,
            input_audience,
        )
        .await?;
    } else {
        transition_node_counters_in_tx(
            conn,
            run.run_uid,
            &settled_task.node_id,
            &settled_task.item_key,
            waiting_status,
            settled_task.status,
        )
        .await?;
    }
    refresh_run_after_wait_settlement_in_conn(conn, run.run_uid, task.task_id, settled_at).await?;
    Ok(TransitionOutcome::Applied(settled_task))
}

pub(super) async fn refresh_run_after_wait_settlement_in_conn(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
    settled_at: DateTime<Utc>,
) -> Result<()> {
    let updated = sqlx::query(
        r#"
        WITH remaining AS (
            SELECT COALESCE(jsonb_agg(reason ORDER BY ordinal), '[]'::JSONB) AS reasons
            FROM moa.execution_run AS run,
                 jsonb_array_elements(run.waiting_reasons)
                     WITH ORDINALITY AS item(reason, ordinal)
            WHERE run.run_uid = $1
              AND COALESCE(reason ->> 'task_id', '') <> $2
        ), next_trigger AS (
            SELECT (
                SELECT due_at FROM moa.execution_trigger
                WHERE run_uid = $1 AND state = 'pending'
                ORDER BY due_at, trigger_uid LIMIT 1
            ) AS due_at
        ), remaining_wait AS (
            SELECT (
                SELECT waiting_since FROM moa.execution_task
                WHERE run_uid = $1
                  AND status IN (
                      'waiting_input', 'waiting_review', 'waiting_signal',
                      'waiting_timer', 'waiting_external', 'waiting_replan'
                  )
                  AND waiting_since IS NOT NULL
                ORDER BY waiting_since, task_id LIMIT 1
            ) AS waiting_since
        )
        UPDATE moa.execution_run AS run
        SET waiting_reasons = remaining.reasons,
            status = CASE
                WHEN run.status IN ('pause_requested', 'pausing', 'paused') THEN run.status
                WHEN run.waiting_input_task_count > 0 THEN 'waiting_input'
                WHEN run.waiting_review_task_count > 0 THEN 'waiting_review'
                WHEN run.waiting_signal_task_count > 0 THEN 'waiting_signal'
                WHEN run.waiting_timer_task_count > 0 THEN 'waiting_timer'
                WHEN run.waiting_external_task_count > 0 THEN 'waiting_external'
                WHEN run.waiting_replan_task_count > 0 THEN 'waiting_replan'
                ELSE 'running'
            END,
            next_wake_at = next_trigger.due_at,
            waiting_since = remaining_wait.waiting_since,
            waiting_reasons_truncated = jsonb_array_length(
                jsonb_path_query_array(
                    remaining.reasons,
                    '$[*] ? (exists(@.task_id))'
                )
            ) < run.waiting_task_count,
            last_progress_at = GREATEST(run.last_progress_at, $3),
            updated_at = NOW()
        FROM remaining, next_trigger, remaining_wait
        WHERE run.run_uid = $1
        "#,
    )
    .bind(run_uid)
    .bind(task_id.as_uuid().to_string())
    .bind(settled_at)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if updated.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "storage wait settlement lost its run projection".to_string(),
        });
    }
    Ok(())
}

fn settlement_for_delivered_trigger(
    task: &ExecutionTaskRecord,
    kind: ExecutionTriggerKind,
) -> Result<WaitSettlement> {
    match kind {
        ExecutionTriggerKind::TaskTimer => match &task.kind {
            LogicalTaskKind::WaitUntil { result, .. } => Ok(WaitSettlement::TimerElapsed {
                task_id: task.task_id,
                output: result.clone(),
            }),
            _ => Err(Error::InvalidRepositoryData {
                message: "task_timer trigger targets a non-timer task".to_string(),
            }),
        },
        ExecutionTriggerKind::WaitExpiry => {
            let action = match (&task.status, &task.kind) {
                (
                    ExecutionTaskStatus::WaitingReview,
                    LogicalTaskKind::Review { wait_policy, .. },
                )
                | (
                    ExecutionTaskStatus::WaitingSignal,
                    LogicalTaskKind::WaitSignal { wait_policy, .. },
                ) => wait_policy.on_expiry.clone(),
                _ => {
                    return Err(Error::InvalidRepositoryData {
                        message: "wait_expiry trigger targets a non-expiring wait".to_string(),
                    });
                }
            };
            Ok(WaitSettlement::WaitExpired {
                task_id: task.task_id,
                action,
            })
        }
        ExecutionTriggerKind::RunDeadline
        | ExecutionTriggerKind::TaskWatchdog
        | ExecutionTriggerKind::ExternalReconcile
        | ExecutionTriggerKind::ExternalStartRecovery
        | ExecutionTriggerKind::ScheduleOccurrence
        | ExecutionTriggerKind::CompensationWatchdog => Err(Error::InvalidRepositoryInput {
            message: "only task wait triggers have storage wait settlements".to_string(),
        }),
    }
}

fn wait_settlement_is_exact_replay(
    task: &ExecutionTaskRecord,
    task_generation: u64,
    waiting_since: DateTime<Utc>,
    settlement: &WaitSettlement,
) -> bool {
    task.status.is_terminal()
        && task.current_outcome.is_some()
        && task.generation_history.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("storage_wait_settlement")
                && entry.get("task_generation").and_then(Value::as_u64) == Some(task_generation)
                && entry
                    .get("waiting_since")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<DateTime<Utc>>(value).ok())
                    == Some(waiting_since)
                && entry
                    .get("settlement")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<WaitSettlement>(value).ok())
                    .as_ref()
                    == Some(settlement)
        })
        && task.outcome_audit.iter().any(|entry| {
            entry.get("received_generation").and_then(Value::as_u64) == Some(task_generation)
                && entry.get("accepted").and_then(Value::as_bool) == Some(true)
        })
}

fn wait_settlement_outcome(
    task: &ExecutionTaskRecord,
    settlement: &WaitSettlement,
) -> Result<ExecutionTaskOutcome> {
    let usage = task.actual.clone();
    match settlement {
        WaitSettlement::TimerElapsed { output, .. } => match &task.kind {
            LogicalTaskKind::WaitUntil { result, .. }
                if task.status == ExecutionTaskStatus::WaitingTimer && result == output =>
            {
                Ok(completed_task_outcome(output.clone(), usage))
            }
            _ => Err(Error::InvalidRepositoryInput {
                message: "timer settlement does not match the persisted waiting task".to_string(),
            }),
        },
        WaitSettlement::WaitExpired { action, .. } => {
            let persisted_action = match (&task.status, &task.kind) {
                (
                    ExecutionTaskStatus::WaitingReview,
                    LogicalTaskKind::Review { wait_policy, .. },
                )
                | (
                    ExecutionTaskStatus::WaitingSignal,
                    LogicalTaskKind::WaitSignal { wait_policy, .. },
                ) => &wait_policy.on_expiry,
                _ => {
                    return Err(Error::InvalidRepositoryInput {
                        message: "wait expiry does not match the persisted waiting task"
                            .to_string(),
                    });
                }
            };
            if persisted_action != action {
                return Err(Error::InvalidRepositoryInput {
                    message: "wait expiry action differs from the immutable compiled plan"
                        .to_string(),
                });
            }
            match action {
                ExecutionWaitExpiryAction::ContinueWith { output } => {
                    Ok(completed_task_outcome(output.clone(), usage))
                }
                ExecutionWaitExpiryAction::FailTask => Ok(failed_task_outcome(
                    ExecutionFailureClass::Terminal,
                    "storage wait expired with fail_task policy".to_string(),
                    usage,
                )),
            }
        }
    }
}

fn wait_settlement_due_at(
    task: &ExecutionTaskRecord,
    settlement: &WaitSettlement,
    waiting_since: DateTime<Utc>,
    run_deadline_at: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let target = match settlement {
        WaitSettlement::TimerElapsed { .. } => match &task.kind {
            LogicalTaskKind::WaitUntil { wake, .. } => wake,
            _ => {
                return Err(Error::InvalidRepositoryInput {
                    message: "timer settlement target is missing".to_string(),
                });
            }
        },
        WaitSettlement::WaitExpired { .. } => match (&task.status, &task.kind) {
            (ExecutionTaskStatus::WaitingReview, LogicalTaskKind::Review { wait_policy, .. })
            | (
                ExecutionTaskStatus::WaitingSignal,
                LogicalTaskKind::WaitSignal { wait_policy, .. },
            ) => &wait_policy.expiry,
            _ => {
                return Err(Error::InvalidRepositoryInput {
                    message: "wait expiry target is missing".to_string(),
                });
            }
        },
    };
    crate::interpreter::resolve_temporal_target(target, waiting_since, run_deadline_at).map_err(
        |_| Error::InvalidRepositoryInput {
            message: "wait expiry target is invalid or not before the run deadline".to_string(),
        },
    )
}

async fn enqueue_pause_cancellations(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    attempt_controller_generation: u64,
    max_active_attempts: u32,
) -> Result<()> {
    let mut matched_cancellations = 0_u64;
    let task_rows = sqlx::query(
        "SELECT task.task_id, task.generation, task.attempt_generation, \
                task.active_dispatch_uid, reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_task AS task \
         JOIN moa.execution_capacity_reservation AS reservation \
           ON reservation.run_uid=task.run_uid AND reservation.task_id=task.task_id \
          AND reservation.attempt_generation=task.attempt_generation \
          AND reservation.controller_generation=$2 \
          AND reservation.state IN ('reserved','reconciling') \
         JOIN moa.execution_trigger AS trigger \
           ON trigger.run_uid=task.run_uid AND trigger.task_id=task.task_id \
          AND trigger.attempt_generation=task.attempt_generation \
          AND trigger.controller_generation=$2 \
          AND trigger.trigger_kind='task_watchdog' \
          AND trigger.state = 'pending' \
         WHERE task.run_uid=$1 AND task.active_dispatch_uid IS NOT NULL \
           AND task.attempt_state IN ('dispatching','running') \
         ORDER BY task.task_id LIMIT $3",
    )
    .bind(run.run_uid)
    .bind(to_i64(
        attempt_controller_generation,
        "attempt controller generation",
    )?)
    .bind(i64::from(max_active_attempts) + 1)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if task_rows.len() > max_active_attempts as usize {
        return Err(Error::InvalidRepositoryData {
            message: "pause found more task attempts than fleet ActiveTasks capacity".to_string(),
        });
    }
    let remaining_limit = max_active_attempts
        .checked_sub(
            u32::try_from(task_rows.len()).map_err(|_| Error::InvalidRepositoryData {
                message: "pause task attempt count does not fit in u32".to_string(),
            })?,
        )
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "pause task attempt count exceeds fleet ActiveTasks capacity".to_string(),
        })?;
    let compensation_rows = sqlx::query(
        "SELECT compensation.compensation_id, compensation.generation, \
                compensation.attempt_generation, compensation.active_dispatch_uid, \
                reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_compensation AS compensation \
         JOIN moa.execution_capacity_reservation AS reservation \
           ON reservation.run_uid=compensation.run_uid \
          AND reservation.compensation_id=compensation.compensation_id \
          AND reservation.compensation_generation=compensation.generation \
          AND reservation.compensation_attempt_generation=compensation.attempt_generation \
          AND reservation.controller_generation=$2 \
          AND reservation.state IN ('reserved','reconciling') \
         JOIN moa.execution_trigger AS trigger \
           ON trigger.run_uid=compensation.run_uid \
          AND trigger.compensation_id=compensation.compensation_id \
          AND trigger.compensation_generation=compensation.generation \
          AND trigger.compensation_attempt_generation=compensation.attempt_generation \
          AND trigger.controller_generation=$2 \
          AND trigger.trigger_kind='compensation_watchdog' \
          AND trigger.state = 'pending' \
         WHERE compensation.run_uid=$1 AND compensation.active_dispatch_uid IS NOT NULL \
           AND compensation.attempt_state IN ('dispatching','running') \
         ORDER BY compensation.compensation_id LIMIT $3",
    )
    .bind(run.run_uid)
    .bind(to_i64(
        attempt_controller_generation,
        "attempt controller generation",
    )?)
    .bind(i64::from(remaining_limit) + 1)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if compensation_rows.len() > remaining_limit as usize {
        return Err(Error::InvalidRepositoryData {
            message: "pause found more combined attempts than fleet ActiveTasks capacity"
                .to_string(),
        });
    }
    let bounded_attempt_count = task_rows
        .len()
        .checked_add(compensation_rows.len())
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "pause active attempt count overflow".to_string(),
        })?;
    if u64::try_from(bounded_attempt_count).map_err(|_| Error::InvalidRepositoryData {
        message: "pause active attempt count does not fit in u64".to_string(),
    })? != run.active_task_count
    {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "pause cancellation matched {bounded_attempt_count} active attempts but run counter records {}",
                run.active_task_count
            ),
        });
    }
    for row in task_rows {
        let task_id: Uuid = row.try_get("task_id").map_err(row_error)?;
        let task_generation = required_u64(&row, "generation")?;
        let attempt_generation = required_u64(&row, "attempt_generation")?;
        let active_dispatch_uid: Uuid = row.try_get("active_dispatch_uid").map_err(row_error)?;
        let capacity_reservation_uid: Uuid = row.try_get("reservation_uid").map_err(row_error)?;
        let watchdog_trigger_uid: Uuid = row.try_get("trigger_uid").map_err(row_error)?;
        let cancellation_dispatch_uid = Uuid::new_v5(
            &PAUSE_CANCEL_NAMESPACE,
            format!(
                "{}:{}:pause_requested",
                active_dispatch_uid, run.controller_generation
            )
            .as_bytes(),
        );
        let payload = serde_json::to_value(ExecutionTaskAttemptCancelRequest {
            cancellation_dispatch_uid,
            tenant_id: run.tenant_id,
            run_uid: run.run_uid,
            task_id: ExecutionTaskId::from_uuid(task_id),
            controller_generation: run.controller_generation,
            attempt_controller_generation,
            task_generation,
            attempt_generation,
            active_dispatch_uid,
            capacity_reservation_uid,
            watchdog_trigger_uid,
            reason: ExecutionAttemptCancelReason::PauseRequested,
        })?;
        enqueue_dispatch_in_conn(
            conn,
            &NewExecutionDispatch {
                dispatch_uid: cancellation_dispatch_uid,
                tenant_id: run.tenant_id,
                run_uid: Some(run.run_uid),
                task_id: Some(task_id),
                compensation_id: None,
                trigger_uid: None,
                external_job_uid: None,
                kind: ExecutionDispatchKind::TaskAttemptCancel,
                controller_generation: Some(run.controller_generation),
                wake_epoch: None,
                attempt_generation: Some(attempt_generation),
                compensation_generation: None,
                compensation_attempt_generation: None,
                not_before_at: Utc::now(),
                payload,
            },
        )
        .await?;
        let cancelling = sqlx::query(
            "UPDATE moa.execution_task SET attempt_state='cancelling', \
                 last_progress_at=NOW(), updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND active_dispatch_uid=$5 \
               AND attempt_state IN ('dispatching','running')",
        )
        .bind(run.run_uid)
        .bind(task_id)
        .bind(to_i64(task_generation, "task cancellation generation")?)
        .bind(to_i64(
            attempt_generation,
            "task cancellation attempt generation",
        )?)
        .bind(active_dispatch_uid)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        if cancelling.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: format!("task `{task_id}` lost its pause cancellation fence"),
            });
        }
        matched_cancellations =
            matched_cancellations
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "pause cancellation count overflow".to_string(),
                })?;
    }

    for row in compensation_rows {
        let compensation_id: Uuid = row.try_get("compensation_id").map_err(row_error)?;
        let compensation_generation = required_u64(&row, "generation")?;
        let compensation_attempt_generation = required_u64(&row, "attempt_generation")?;
        let active_dispatch_uid: Uuid = row.try_get("active_dispatch_uid").map_err(row_error)?;
        let capacity_reservation_uid: Uuid = row.try_get("reservation_uid").map_err(row_error)?;
        let watchdog_trigger_uid: Uuid = row.try_get("trigger_uid").map_err(row_error)?;
        let cancellation_dispatch_uid = Uuid::new_v5(
            &PAUSE_CANCEL_NAMESPACE,
            format!(
                "{}:{}:pause_requested",
                active_dispatch_uid, run.controller_generation
            )
            .as_bytes(),
        );
        let payload = serde_json::to_value(ExecutionCompensationAttemptCancelRequest {
            cancellation_dispatch_uid,
            tenant_id: run.tenant_id,
            run_uid: run.run_uid,
            compensation_id: CompensationId::from_uuid(compensation_id),
            controller_generation: run.controller_generation,
            attempt_controller_generation,
            compensation_generation,
            compensation_attempt_generation,
            active_dispatch_uid,
            capacity_reservation_uid,
            watchdog_trigger_uid,
            intent: crate::wire::ExecutionCompensationReleaseIntent::Pause,
        })?;
        enqueue_dispatch_in_conn(
            conn,
            &NewExecutionDispatch {
                dispatch_uid: cancellation_dispatch_uid,
                tenant_id: run.tenant_id,
                run_uid: Some(run.run_uid),
                task_id: None,
                compensation_id: Some(compensation_id),
                trigger_uid: None,
                external_job_uid: None,
                kind: ExecutionDispatchKind::CompensationAttemptCancel,
                controller_generation: Some(run.controller_generation),
                wake_epoch: None,
                attempt_generation: None,
                compensation_generation: Some(compensation_generation),
                compensation_attempt_generation: Some(compensation_attempt_generation),
                not_before_at: Utc::now(),
                payload,
            },
        )
        .await?;
        let cancelling = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
                 release_intent='pause', last_progress_at=clock_timestamp(), \
                 updated_at=clock_timestamp() \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND active_dispatch_uid=$5 \
               AND attempt_state IN ('dispatching','running')",
        )
        .bind(run.run_uid)
        .bind(compensation_id)
        .bind(to_i64(
            compensation_generation,
            "compensation cancellation generation",
        )?)
        .bind(to_i64(
            compensation_attempt_generation,
            "compensation cancellation attempt generation",
        )?)
        .bind(active_dispatch_uid)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        if cancelling.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "compensation `{compensation_id}` lost its pause cancellation fence"
                ),
            });
        }
        matched_cancellations =
            matched_cancellations
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "pause cancellation count overflow".to_string(),
                })?;
    }
    debug_assert_eq!(matched_cancellations, run.active_task_count);
    Ok(())
}

pub(super) fn task_outcome_is_exact_replay(
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
) -> bool {
    task.current_outcome.as_ref() == Some(outcome)
        && task.outcome_audit.iter().any(|entry| {
            entry.get("received_generation").and_then(Value::as_u64) == Some(generation)
                && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                && entry
                    .get("outcome")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ExecutionTaskOutcome>(value).ok())
                    .as_ref()
                    == Some(outcome)
        })
}
