//! Execution-run creation, lookup, pagination, and scheduling snapshots.

use super::*;
use super::{
    capacity::{
        ActiveRunCapacityReserveOutcome, CapacityReserveOutcome, ExecutionCapacityDimension,
        ExecutionCapacityOwner, ExecutionCapacityRequest, execution_capacity_reservation_uid,
        prelock_capacity_dimensions_in_tx, reserve_active_run_capacity_in_tx,
        transfer_active_run_to_parked_in_tx, transfer_parked_run_to_active_in_tx,
    },
    outbox::{
        ExecutionDispatchKind, ExecutionDispatchRecord, NewExecutionDispatch,
        dispatch_from_row_for_repository, enqueue_dispatch_in_conn,
    },
    rows::*,
    sql::*,
    trigger::{
        ExecutionTriggerKind, ExecutionTriggerSupersedeOutcome, NewExecutionTrigger,
        create_trigger_with_dispatch_in_conn, supersede_trigger_in_conn,
    },
};
use moa_config::ExecutionConfig;

const RUN_ACTIVATION_DISPATCH_NAMESPACE: Uuid =
    Uuid::from_u128(0x83f0_a3b7_6f50_5c12_99d0_48a0_3b09_2cd4);
const RUN_DEADLINE_TRIGGER_NAMESPACE: Uuid =
    Uuid::from_u128(0xb14e_032e_1b46_5e32_82b0_999d_1d45_9cb2);
const RUN_LIFETIME_CAPACITY_GENERATION: u64 = 1;
const LOAD_RUN_FOR_SESSION_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE run_uid = $1
      AND session_id = $2
"#;

/// Bounded cancellation evidence loaded under the caller's exact session fence.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionCancellationProjection {
    /// Current durable run projection.
    pub run: ExecutionRunRecord,
    /// Completed plan-node identities, bounded by the compiler-capped active plan.
    pub completed_node_ids: Vec<String>,
    /// Exact terminal-cancellation receipts still joined to a current cancelling task attempt.
    pub task_cancellation_dispatches: Vec<ExecutionDispatchRecord>,
}
const LOAD_RUN_BY_IDEMPOTENCY_FOR_SESSION_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND idempotency_key = $3
      AND session_id = $4
"#;

/// Recovery request for one crashed controller activation of an exact wake.
#[derive(Clone, Debug, PartialEq)]
pub struct ResumedControllerRecoveryRequest {
    /// Exact controller generation claimed by the crashed activation.
    pub controller_generation: u64,
    /// Exact wake epoch claimed by the crashed activation.
    pub wake_epoch: u64,
    /// Durable checkpoint installed while the replacement activation is enqueued.
    pub checkpoint: ExecutionRunActivationCheckpoint,
    /// Structured activation payload carried by the replacement activation.
    pub continuation_payload: Value,
    /// Earliest time at which the replacement activation may be dispatched.
    pub continuation_not_before_at: DateTime<Utc>,
    /// Consecutive crashed activations tolerated before the run must fail instead.
    pub maximum_consecutive_failures: u64,
}

/// Bounded outcome of one resumed-activation recovery.
#[derive(Clone, Debug, PartialEq)]
pub enum ResumedControllerRecoveryOutcome {
    /// The wake was acknowledged and exactly one replacement activation enqueued.
    Recovered {
        /// Current run after the commit.
        run: Box<ExecutionRunRecord>,
        /// Exactly one replacement activation committed with the checkpoint.
        continuation: Box<ExecutionDispatchRecord>,
        /// Consecutive crashed activations recorded by this recovery.
        consecutive_failures: u64,
    },
    /// The recovery budget is spent; neither the wake nor the failure count was mutated.
    BudgetExhausted {
        /// Consecutive crashed activations that would have been recorded.
        consecutive_failures: u64,
    },
    /// The claimed wake had already been acknowledged.
    Replayed(Box<ExecutionRunRecord>),
    /// A newer controller generation owns the run.
    StaleGeneration {
        /// Controller generation currently owning the run.
        current_generation: u64,
    },
    /// A newer wake epoch owns the run.
    StaleWake {
        /// Wake epoch currently owning the run.
        current_wake_epoch: u64,
        /// Last wake epoch acknowledged by compare-and-set.
        processed_wake_epoch: u64,
    },
    /// The run no longer exists.
    NotFound,
    /// The run is not in a state that can acknowledge the claimed wake.
    InvalidState,
}

/// Atomic run-admission result, including durable idempotency replay and capacity deferral.
#[derive(Clone, Debug, PartialEq)]
pub enum RunAdmissionOutcome {
    /// A new run, its scheduler state, and its lifetime capacity receipt committed together.
    Admitted(Box<ExecutionRunRecord>),
    /// The exact scoped idempotency key already owns this admitted run.
    Replayed(Box<ExecutionRunRecord>),
    /// Fleet or tenant admission capacity is exhausted; no run row committed.
    CapacitySaturated {
        /// The lifetime capacity dimension that rejected admission.
        dimension: ExecutionCapacityDimension,
    },
}

impl ExecutionRepository {
    /// Admits a run, scheduler state, and lifetime capacity in one transaction.
    pub async fn create_run(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        new_run: NewExecutionRun,
    ) -> Result<RunAdmissionOutcome> {
        validate_new_run(scope, &new_run)?;
        let budget = DbBudgetLimit::try_from(&new_run.approved_budget)?;
        let run_uid = Uuid::now_v7();
        let plan_value = serde_json::to_value(&new_run.plan)?;
        let goal_value = serde_json::to_value(&new_run.goal)?;
        let admitted_identity_value = serde_json::to_value(&new_run.admitted_identity)?;
        let catalog_value = serde_json::to_value(&new_run.catalog)?;
        let authorization_value = serde_json::to_value(&new_run.authorization)?;
        let pinned_skills_value = serde_json::to_value(&new_run.pinned_instruction_skills)?;
        let source_provenance_value = serde_json::to_value(&new_run.source_provenance)?;
        let source_fields = normalized_source_fields(&new_run.source_provenance);
        let activation_state = initial_activation_state(new_run.status)?;
        let originating_user_sequence_num = to_i64(
            new_run.originating_user_sequence_num,
            "originating user sequence",
        )?;
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(CREATE_RUN_SQL)
            .bind(run_uid)
            .bind(new_run.tenant_id.0)
            .bind(new_run.contact_id.map(|value| value.0))
            .bind(new_run.session_id.0)
            .bind(originating_user_sequence_num)
            .bind(new_run.planning_context_uid)
            .bind(new_run.planning_context_hash.to_string())
            .bind(new_run.owner_user_id.as_str())
            .bind(admitted_identity_value)
            .bind(goal_value)
            .bind(&plan_value)
            .bind(&plan_value)
            .bind(new_run.plan.plan_hash.to_string())
            .bind(new_run.plan.plan_hash.to_string())
            .bind(catalog_value)
            .bind(authorization_value)
            .bind(pinned_skills_value)
            .bind(source_provenance_value)
            .bind(source_fields.kind.as_str())
            .bind(source_fields.skill_template_ref)
            .bind(source_fields.skill_template_revision_uid)
            .bind(new_run.input)
            .bind(new_run.status.as_str())
            .bind(budget.max_cost_microusd)
            .bind(budget.max_tokens)
            .bind(budget.max_tasks)
            .bind(budget.max_tool_calls)
            .bind(budget.max_retrieved_bytes)
            .bind(budget.deadline_at)
            .bind(0_i64)
            .bind(new_run.idempotency_key.as_deref())
            .bind(activation_state.as_str())
            .bind(Option::<Uuid>::None)
            .bind(Option::<i64>::None)
            .bind(Option::<i64>::None)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;

        let (record, admitted) = if let Some(row) = row {
            (run_from_row(&row)?, true)
        } else if let Some(idempotency_key) = new_run.idempotency_key.as_deref() {
            let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
                .bind(new_run.tenant_id.0)
                .bind(new_run.contact_id.map(|value| value.0))
                .bind(idempotency_key)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                .ok_or_else(|| Error::Storage {
                    message: "idempotent run insert conflicted without a visible existing row"
                        .to_string(),
                })?;
            (run_from_row(&row)?, false)
        } else {
            return Err(Error::Storage {
                message: "execution run insert conflicted without an idempotency key".to_string(),
            });
        };
        if record.admitted_identity != new_run.admitted_identity {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message:
                    "execution idempotency key is already bound to a different admitted identity"
                        .to_string(),
            });
        }
        if !admitted {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunAdmissionOutcome::Replayed(Box::new(record)));
        }

        seed_run_scheduler_state_in_tx(
            conn.as_mut(),
            record.tenant_id,
            record.run_uid,
            &record.active_plan,
        )
        .await?;
        let mut capacity_dimensions = vec![
            ExecutionCapacityDimension::ActiveRuns,
            ExecutionCapacityDimension::ParkedRuns,
        ];
        if record.approved_budget.deadline_at.is_some() {
            capacity_dimensions.push(ExecutionCapacityDimension::ScheduledTriggers);
        }
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            record.tenant_id,
            &capacity_dimensions,
        )
        .await?;
        let capacity = reserve_active_run_capacity_in_tx(
            conn.as_mut(),
            config,
            active_run_capacity_request(record.tenant_id, record.run_uid),
        )
        .await?;
        match capacity {
            ActiveRunCapacityReserveOutcome::Reserved
            | ActiveRunCapacityReserveOutcome::Replayed => {}
            ActiveRunCapacityReserveOutcome::Saturated(dimension) => {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(RunAdmissionOutcome::CapacitySaturated { dimension });
            }
        }
        match arm_run_deadline_in_conn(conn.as_mut(), config, &record).await {
            Ok(
                RunDeadlineArmOutcome::Armed(_)
                | RunDeadlineArmOutcome::NoDeadline
                | RunDeadlineArmOutcome::Terminal,
            ) => {}
            Ok(RunDeadlineArmOutcome::NotFound | RunDeadlineArmOutcome::StaleGeneration { .. }) => {
                conn.rollback().await.map_err(storage_error)?;
                return Err(Error::InvalidRepositoryData {
                    message: "newly inserted execution run lost its deadline arm fence".to_string(),
                });
            }
            Err(Error::CapacitySaturated { dimension })
                if dimension == ExecutionCapacityDimension::ScheduledTriggers.as_str() =>
            {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(RunAdmissionOutcome::CapacitySaturated {
                    dimension: ExecutionCapacityDimension::ScheduledTriggers,
                });
            }
            Err(error) => return Err(error),
        }
        if record.status == ExecutionRunStatus::Queued {
            enqueue_dispatch_in_conn(
                conn.as_mut(),
                &NewExecutionDispatch {
                    dispatch_uid: run_activation_dispatch_uid(
                        record.run_uid,
                        record.controller_generation,
                        record.wake_epoch,
                    ),
                    tenant_id: record.tenant_id,
                    run_uid: Some(record.run_uid),
                    task_id: None,
                    compensation_id: None,
                    trigger_uid: None,
                    external_job_uid: None,
                    kind: ExecutionDispatchKind::RunActivation,
                    controller_generation: Some(record.controller_generation),
                    wake_epoch: Some(record.wake_epoch),
                    attempt_generation: None,
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    not_before_at: record.created_at,
                    payload: json!({"reason": "run_admitted"}),
                },
            )
            .await?;
        }
        let row = sqlx::query(LOAD_RUN_SQL)
            .bind(record.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let record = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(RunAdmissionOutcome::Admitted(Box::new(record)))
    }

    /// Loads one visible execution run.
    pub async fn load_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionRunRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads one visible execution run only when it belongs to the expected parent session.
    pub async fn load_run_for_session(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_session_id: SessionId,
    ) -> Result<Option<ExecutionRunRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_FOR_SESSION_SQL)
            .bind(run_uid)
            .bind(expected_session_id.0)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads one session-fenced run plus bounded completed-node cancellation evidence.
    pub async fn load_cancellation_projection_for_session(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_session_id: SessionId,
        max_current_task_owners: usize,
    ) -> Result<Option<ExecutionCancellationProjection>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_FOR_SESSION_SQL)
            .bind(run_uid)
            .bind(expected_session_id.0)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let run = run_from_row(&row)?;
        let plan_node_ids = run
            .active_plan
            .definition
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let fetch_limit = i64::try_from(plan_node_ids.len())
            .map_err(|_| Error::InvalidRepositoryData {
                message: "active plan node count exceeds PostgreSQL BIGINT".to_string(),
            })?
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "cancellation projection node limit".to_string(),
            })?;
        let completed_node_ids = sqlx::query_scalar::<_, String>(
            "SELECT node_id FROM moa.execution_node_state \
             WHERE run_uid=$1 AND node_id=ANY($2::TEXT[]) AND node_status='completed' \
             ORDER BY node_order, node_id LIMIT $3",
        )
        .bind(run_uid)
        .bind(&plan_node_ids)
        .bind(fetch_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if completed_node_ids.len() > plan_node_ids.len() {
            return Err(Error::InvalidRepositoryData {
                message: "cancellation projection exceeded its active-plan bound".to_string(),
            });
        }
        let task_cancellation_dispatches = if run.status == ExecutionRunStatus::Cancelled
            || run
                .pending_terminal
                .as_ref()
                .is_some_and(|pending| pending.status == ExecutionRunStatus::Cancelled)
        {
            load_current_terminal_task_cancellation_dispatches(
                &mut conn,
                &run,
                max_current_task_owners,
            )
            .await?
        } else {
            Vec::new()
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(Some(ExecutionCancellationProjection {
            run,
            completed_node_ids,
            task_cancellation_dispatches,
        }))
    }

    /// Loads one visible task under its owning run and stable task ID.
    pub async fn load_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
    ) -> Result<Option<ExecutionTaskRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(task_from_row).transpose()
    }

    /// Loads a visible run for one scope-local idempotency key.
    pub async fn load_run_by_idempotency_key(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        idempotency_key: &str,
    ) -> Result<Option<ExecutionRunRecord>> {
        if !scope.permits_owner(tenant_id, contact_id) {
            return Err(Error::InvalidRepositoryInput {
                message: "idempotency lookup owner does not match repository scope".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_SQL)
            .bind(tenant_id.0)
            .bind(contact_id.map(|value| value.0))
            .bind(idempotency_key)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Loads a scope-local idempotent run only when it belongs to the expected session.
    pub async fn load_run_by_idempotency_key_for_session(
        &self,
        scope: ExecutionScope,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        expected_session_id: SessionId,
        idempotency_key: &str,
    ) -> Result<Option<ExecutionRunRecord>> {
        if !scope.permits_owner(tenant_id, contact_id) {
            return Err(Error::InvalidRepositoryInput {
                message: "idempotency lookup owner does not match repository scope".to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(LOAD_RUN_BY_IDEMPOTENCY_FOR_SESSION_SQL)
            .bind(tenant_id.0)
            .bind(contact_id.map(|value| value.0))
            .bind(idempotency_key)
            .bind(expected_session_id.0)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Lists one bounded, stable page of visible execution runs.
    pub async fn list_runs(
        &self,
        scope: ExecutionScope,
        page: ExecutionRunPageRequest,
    ) -> Result<ExecutionRunPage> {
        let limit = if page.limit == 0 {
            DEFAULT_RUN_PAGE_LIMIT
        } else {
            page.limit.min(MAX_RUN_PAGE_LIMIT)
        };
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(LIST_RUNS_SQL)
            .bind(page.cursor.map(|cursor| cursor.created_at))
            .bind(page.cursor.map(|cursor| cursor.run_uid))
            .bind(i64::from(limit) + 1)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut runs = rows.iter().map(run_from_row).collect::<Result<Vec<_>>>()?;
        let has_more = runs.len() > limit as usize;
        if has_more {
            let _ = runs.pop();
        }
        let next_cursor = if has_more {
            runs.last().map(|run| ExecutionRunCursor {
                created_at: run.created_at,
                run_uid: run.run_uid,
            })
        } else {
            None
        };
        Ok(ExecutionRunPage { runs, next_cursor })
    }

    /// Claims one exact queued controller generation for a bounded activation.
    pub async fn claim_run_activation(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
    ) -> Result<RunActivationWriteOutcome> {
        let generation = to_i64(controller_generation, "controller generation")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(locked_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::NotFound);
        };
        let locked = run_from_row(&locked_row)?;
        if locked.controller_generation != controller_generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::GenerationMismatch);
        }
        if locked.activation_state == ExecutionActivationState::Advancing {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::AlreadyApplied(locked));
        }
        if locked.activation_state != ExecutionActivationState::Queued
            || locked.status.is_terminal()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::InvalidState);
        }
        let row = sqlx::query(CLAIM_RUN_ACTIVATION_SQL)
            .bind(run_uid)
            .bind(generation)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: "run activation claim lost its locked generation fence".to_string(),
            })?;
        let record = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(RunActivationWriteOutcome::Applied(record))
    }

    /// Persists the exact terminal or parked checkpoint produced by one activation.
    pub async fn checkpoint_run_activation(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        checkpoint: ExecutionRunActivationCheckpoint,
    ) -> Result<RunActivationWriteOutcome> {
        validate_activation_checkpoint(&checkpoint)?;
        let generation = to_i64(controller_generation, "controller generation")?;
        let ready_task_count = to_i64(checkpoint.ready_task_count, "ready task count")?;
        let active_task_count = to_i64(checkpoint.active_task_count, "active task count")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(locked_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::NotFound);
        };
        let locked = run_from_row(&locked_row)?;
        if locked.controller_generation != controller_generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::GenerationMismatch);
        }
        if activation_checkpoint_matches(&locked, &checkpoint) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::AlreadyApplied(locked));
        }
        if locked.activation_state != ExecutionActivationState::Advancing {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunActivationWriteOutcome::InvalidState);
        }
        let row = sqlx::query(CHECKPOINT_RUN_ACTIVATION_SQL)
            .bind(run_uid)
            .bind(generation)
            .bind(checkpoint.status.as_str())
            .bind(checkpoint.activation_state.as_str())
            .bind(checkpoint.next_wake_at)
            .bind(checkpoint.waiting_since)
            .bind(ready_task_count)
            .bind(active_task_count)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: "run activation checkpoint lost its locked generation fence".to_string(),
            })?;
        let record = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(RunActivationWriteOutcome::Applied(record))
    }

    /// Claims one exact controller generation and unprocessed wake epoch atomically.
    pub async fn claim_controller_wake(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        wake_epoch: u64,
    ) -> Result<RunControllerClaimOutcome> {
        let generation = to_i64(controller_generation, "controller generation")?;
        let wake = to_i64(wake_epoch, "wake epoch")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunControllerClaimOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        let outcome = if run.controller_generation != controller_generation {
            RunControllerClaimOutcome::StaleGeneration {
                current_generation: run.controller_generation,
            }
        } else if run.status.is_terminal()
            || run.activation_state == ExecutionActivationState::Terminal
        {
            RunControllerClaimOutcome::Terminal(run)
        } else if wake_epoch <= run.processed_wake_epoch {
            RunControllerClaimOutcome::Replayed(run)
        } else if run.wake_epoch != wake_epoch {
            RunControllerClaimOutcome::StaleWake {
                current_wake_epoch: run.wake_epoch,
                processed_wake_epoch: run.processed_wake_epoch,
            }
        } else if run.activation_state == ExecutionActivationState::Advancing {
            RunControllerClaimOutcome::Resumed(run)
        } else if run.activation_state != ExecutionActivationState::Queued {
            RunControllerClaimOutcome::InvalidState
        } else {
            let updated = sqlx::query(
                "UPDATE moa.execution_run \
                 SET activation_state = 'advancing', updated_at = NOW() \
                 WHERE run_uid = $1 AND controller_generation = $2 AND wake_epoch = $3 \
                   AND processed_wake_epoch < $3 AND activation_state = 'queued' \
                 RETURNING *",
            )
            .bind(run_uid)
            .bind(generation)
            .bind(wake)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: "controller wake claim lost its locked compare-and-set".to_string(),
            })?;
            RunControllerClaimOutcome::Claimed(run_from_row(&updated)?)
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Checkpoints one exact wake, acknowledges it, and optionally enqueues one continuation.
    pub async fn complete_controller_wake(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        request: RunControllerCompletionRequest,
    ) -> Result<RunControllerCompletionOutcome> {
        validate_activation_checkpoint(&request.checkpoint)?;
        validate_controller_completion(&request)?;
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
            return Ok(RunControllerCompletionOutcome::NotFound);
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
        let checkpoint = if request.continuation_payload.is_some() {
            ExecutionRunActivationCheckpoint {
                activation_state: ExecutionActivationState::Idle,
                ..request.checkpoint.clone()
            }
        } else {
            request.checkpoint.clone()
        };
        let outcome = complete_controller_wake_in_conn(
            &mut conn,
            run_uid,
            request.controller_generation,
            request.wake_epoch,
            checkpoint,
        )
        .await?;
        let outcome = match (outcome, request.continuation_payload) {
            (RunControllerCompletionOutcome::Applied { run, .. }, Some(payload)) => {
                let continuation = enqueue_run_activation_in_conn(
                    conn.as_mut(),
                    run.tenant_id,
                    run_uid,
                    request.controller_generation,
                    request.continuation_not_before_at,
                    payload,
                )
                .await?;
                let row = sqlx::query(LOAD_RUN_SQL)
                    .bind(run_uid)
                    .fetch_one(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                RunControllerCompletionOutcome::Applied {
                    run: Box::new(run_from_row(&row)?),
                    continuation: Some(Box::new(continuation)),
                }
            }
            (RunControllerCompletionOutcome::Applied { run, .. }, None)
                if requires_parked_run_capacity(&run) =>
            {
                if transfer_active_run_to_parked_in_tx(
                    conn.as_mut(),
                    config,
                    &run,
                    request.wake_epoch,
                )
                .await?
                    == CapacityReserveOutcome::Saturated
                {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(RunControllerCompletionOutcome::CapacitySaturated {
                        dimension: ExecutionCapacityDimension::ParkedRuns,
                    });
                }
                RunControllerCompletionOutcome::Applied {
                    run,
                    continuation: None,
                }
            }
            (outcome, _) => outcome,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Recovers one crashed controller activation under a bounded consecutive-failure budget.
    ///
    /// A resumed claim proves that a prior activation of this exact wake never acknowledged it.
    /// While the budget holds, the wake is acknowledged and exactly one replacement activation is
    /// enqueued in the same transaction. Once the budget is spent the wake is deliberately left
    /// unacknowledged and [`ResumedControllerRecoveryOutcome::BudgetExhausted`] is returned, so the
    /// caller can commit an explicit terminal intent against the same still-current wake instead of
    /// re-enqueueing a continuation that can only crash again.
    pub async fn recover_resumed_controller_wake(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        request: ResumedControllerRecoveryRequest,
    ) -> Result<ResumedControllerRecoveryOutcome> {
        validate_activation_checkpoint(&request.checkpoint)?;
        if !request.continuation_payload.is_object() {
            return Err(Error::InvalidRepositoryInput {
                message: "controller continuation payload must be a JSON object".to_string(),
            });
        }
        if request.maximum_consecutive_failures == 0 {
            return Err(Error::InvalidRepositoryInput {
                message: "resumed activation recovery budget must be greater than zero".to_string(),
            });
        }
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
            return Ok(ResumedControllerRecoveryOutcome::NotFound);
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
        let observed = sqlx::query_scalar::<_, i64>(
            "SELECT activation_failure_count FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(observed) = observed else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ResumedControllerRecoveryOutcome::NotFound);
        };
        let consecutive_failures = to_u64(observed, "activation failure count")?
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "controller activation failure count".to_string(),
            })?;
        if consecutive_failures > request.maximum_consecutive_failures {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ResumedControllerRecoveryOutcome::BudgetExhausted {
                consecutive_failures,
            });
        }
        let checkpoint = ExecutionRunActivationCheckpoint {
            activation_state: ExecutionActivationState::Idle,
            ..request.checkpoint
        };
        let completion = complete_controller_wake_in_conn(
            &mut conn,
            run_uid,
            request.controller_generation,
            request.wake_epoch,
            checkpoint,
        )
        .await?;
        let outcome = match completion {
            RunControllerCompletionOutcome::Applied { .. } => {
                record_activation_failure_count_in_conn(
                    conn.as_mut(),
                    run_uid,
                    consecutive_failures,
                )
                .await?;
                let continuation = enqueue_run_activation_in_conn(
                    conn.as_mut(),
                    TenantId(tenant_id),
                    run_uid,
                    request.controller_generation,
                    request.continuation_not_before_at,
                    request.continuation_payload,
                )
                .await?;
                let row = sqlx::query(LOAD_RUN_SQL)
                    .bind(run_uid)
                    .fetch_one(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                ResumedControllerRecoveryOutcome::Recovered {
                    run: Box::new(run_from_row(&row)?),
                    continuation: Box::new(continuation),
                    consecutive_failures,
                }
            }
            RunControllerCompletionOutcome::Replayed(run) => {
                ResumedControllerRecoveryOutcome::Replayed(run)
            }
            RunControllerCompletionOutcome::StaleGeneration { current_generation } => {
                ResumedControllerRecoveryOutcome::StaleGeneration { current_generation }
            }
            RunControllerCompletionOutcome::StaleWake {
                current_wake_epoch,
                processed_wake_epoch,
            } => ResumedControllerRecoveryOutcome::StaleWake {
                current_wake_epoch,
                processed_wake_epoch,
            },
            RunControllerCompletionOutcome::NotFound => ResumedControllerRecoveryOutcome::NotFound,
            RunControllerCompletionOutcome::InvalidState => {
                ResumedControllerRecoveryOutcome::InvalidState
            }
            RunControllerCompletionOutcome::CapacitySaturated { dimension } => {
                conn.rollback().await.map_err(storage_error)?;
                return Err(Error::CapacitySaturated {
                    dimension: dimension.as_str(),
                });
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Arms or idempotently replaces the exact deadline trigger for a run generation.
    pub async fn arm_run_deadline(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        config: &ExecutionConfig,
    ) -> Result<RunDeadlineArmOutcome> {
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
            return Ok(RunDeadlineArmOutcome::NotFound);
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[ExecutionCapacityDimension::ScheduledTriggers],
        )
        .await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunDeadlineArmOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        if run.tenant_id.0 != tenant_id {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "run deadline tenant changed between capacity prelock and row lock"
                    .to_string(),
            });
        }
        if run.controller_generation != controller_generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunDeadlineArmOutcome::StaleGeneration {
                current_generation: run.controller_generation,
            });
        }
        if run.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunDeadlineArmOutcome::Terminal);
        }
        let outcome = arm_run_deadline_in_conn(conn.as_mut(), config, &run).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

pub(super) async fn load_current_terminal_task_cancellation_dispatches(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    max_current_task_owners: usize,
) -> Result<Vec<ExecutionDispatchRecord>> {
    let fetch_limit = i64::try_from(max_current_task_owners)
        .map_err(|_| Error::InvalidRepositoryInput {
            message: "cancellation owner bound exceeds PostgreSQL BIGINT".to_string(),
        })?
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "cancellation owner projection limit".to_string(),
        })?;
    let rows = sqlx::query(
        "SELECT dispatch.* FROM moa.execution_dispatch_outbox AS dispatch \
         JOIN moa.execution_task AS task ON task.run_uid=dispatch.run_uid \
          AND task.task_id=dispatch.task_id \
         WHERE dispatch.run_uid=$1 AND dispatch.tenant_id=$2 \
          AND dispatch.dispatch_kind='task_attempt_cancel' \
          AND dispatch.controller_generation=$3 \
          AND dispatch.attempt_generation=task.attempt_generation \
          AND task.attempt_state='cancelling' \
          AND task.active_dispatch_uid IS NOT NULL \
          AND dispatch.payload->>'active_dispatch_uid'=task.active_dispatch_uid::TEXT \
          AND dispatch.payload->>'task_generation'=task.generation::TEXT \
          AND dispatch.payload->>'attempt_generation'=task.attempt_generation::TEXT \
          AND dispatch.payload->>'controller_generation'=$3::TEXT \
          AND dispatch.payload->>'reason'='run_terminal' \
         ORDER BY dispatch.task_id, dispatch.dispatch_uid LIMIT $4",
    )
    .bind(run.run_uid)
    .bind(run.tenant_id.0)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(fetch_limit)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if rows.len() > max_current_task_owners {
        return Err(Error::InvalidRepositoryData {
            message: "cancellation owner projection exceeded its configured in-flight bound"
                .to_string(),
        });
    }
    rows.iter().map(dispatch_from_row_for_repository).collect()
}

/// Arms the exact immutable run-deadline trigger inside the caller's transaction.
///
/// Multi-resource admission callers must prelock `ActiveRuns` then `ScheduledTriggers` before
/// invoking this helper. The helper never commits, so run admission, capacity, delayed delivery,
/// and the run wake projection share one crash-safe boundary.
pub(super) async fn arm_run_deadline_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
) -> Result<RunDeadlineArmOutcome> {
    if run.status.is_terminal() {
        return Ok(RunDeadlineArmOutcome::Terminal);
    }
    if run.budget_deadline_suspended_at.is_some() {
        return Ok(RunDeadlineArmOutcome::NoDeadline);
    }
    let Some(deadline_at) = run.approved_budget.deadline_at else {
        return Ok(RunDeadlineArmOutcome::NoDeadline);
    };
    let trigger_uid = run_deadline_trigger_uid(run.run_uid, run.controller_generation, deadline_at);
    let stale_deadlines = sqlx::query(
        "SELECT trigger_uid, controller_generation \
         FROM moa.execution_trigger \
         WHERE run_uid = $1 AND trigger_kind = 'run_deadline' \
           AND state = 'pending' AND trigger_uid <> $2 \
         ORDER BY controller_generation, trigger_uid \
         LIMIT 2 FOR UPDATE",
    )
    .bind(run.run_uid)
    .bind(trigger_uid)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if stale_deadlines.len() > 1 {
        return Err(Error::InvalidRepositoryData {
            message: "execution run owns multiple stale active deadline triggers".to_string(),
        });
    }
    for stale in stale_deadlines {
        let stale_trigger_uid = stale.try_get::<Uuid, _>("trigger_uid").map_err(row_error)?;
        let stale_generation = required_u64(&stale, "controller_generation")?;
        match supersede_trigger_in_conn(
            conn,
            stale_trigger_uid,
            ExecutionTriggerKind::RunDeadline,
            Some(stale_generation),
            None,
            None,
            None,
        )
        .await?
        {
            ExecutionTriggerSupersedeOutcome::Superseded
            | ExecutionTriggerSupersedeOutcome::AlreadySuperseded
            | ExecutionTriggerSupersedeOutcome::AlreadyInactive => {}
            ExecutionTriggerSupersedeOutcome::StaleOrMissing => {
                return Err(Error::InvalidRepositoryData {
                    message: "locked run deadline disappeared before capacity release".to_string(),
                });
            }
        }
    }
    let write = create_trigger_with_dispatch_in_conn(
        conn,
        config,
        &NewExecutionTrigger {
            trigger_uid,
            tenant_id: run.tenant_id,
            run_uid: Some(run.run_uid),
            task_id: None,
            compensation_id: None,
            schedule_uid: None,
            kind: ExecutionTriggerKind::RunDeadline,
            controller_generation: Some(run.controller_generation),
            attempt_generation: None,
            compensation_generation: None,
            compensation_attempt_generation: None,
            schedule_incarnation: None,
            occurrence_sequence: None,
            due_at: deadline_at,
            payload: json!({ "run_uid": run.run_uid, "deadline_at": deadline_at }),
        },
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET next_wake_at = CASE \
             WHEN next_wake_at IS NULL THEN $3 ELSE LEAST(next_wake_at, $3) END, \
             updated_at = NOW() WHERE run_uid = $1 AND controller_generation = $2",
    )
    .bind(run.run_uid)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(deadline_at)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(RunDeadlineArmOutcome::Armed(Box::new(write)))
}

/// Increments one current run's wake epoch and enqueues its exact activation atomically.
///
/// The caller must already own a transaction that makes the cause idempotent. This helper
/// deliberately does not commit, so trigger delivery, task settlement, and controller
/// continuation can share one persist-before-dispatch boundary.
pub async fn enqueue_run_activation_in_conn(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
    controller_generation: u64,
    not_before_at: DateTime<Utc>,
    payload: Value,
) -> Result<ExecutionDispatchRecord> {
    if !payload.is_object() {
        return Err(Error::InvalidRepositoryInput {
            message: "run activation payload must be a JSON object".to_string(),
        });
    }
    if transfer_parked_run_to_active_in_tx(conn, tenant_id, run_uid, controller_generation).await?
        == CapacityReserveOutcome::Saturated
    {
        return Err(Error::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ActiveRuns.as_str(),
        });
    }
    let generation = to_i64(controller_generation, "controller generation")?;
    let wake_epoch = sqlx::query_scalar::<_, i64>(
        "UPDATE moa.execution_run \
         SET wake_epoch = wake_epoch + 1, activation_state = 'queued', updated_at = NOW() \
         WHERE run_uid = $1 AND tenant_id = $2 AND controller_generation = $3 \
           AND status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled') \
         RETURNING wake_epoch",
    )
    .bind(run_uid)
    .bind(tenant_id.0)
    .bind(generation)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryInput {
        message: "run activation target is missing, terminal, or generation-stale".to_string(),
    })?;
    let wake_epoch = to_u64(wake_epoch, "wake epoch")?;
    let dispatch_uid = run_activation_dispatch_uid(run_uid, controller_generation, wake_epoch);
    enqueue_dispatch_in_conn(
        conn,
        &NewExecutionDispatch {
            dispatch_uid,
            tenant_id,
            run_uid: Some(run_uid),
            task_id: None,
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::RunActivation,
            controller_generation: Some(controller_generation),
            wake_epoch: Some(wake_epoch),
            attempt_generation: None,
            compensation_generation: None,
            compensation_attempt_generation: None,
            not_before_at,
            payload,
        },
    )
    .await
}

/// Checkpoints and acknowledges one exact claimed wake inside the caller's transaction.
///
/// This is the shared boundary for deadline/cancellation drain transactions. Callers that need a
/// continuation must invoke [`enqueue_run_activation_in_conn`] before committing, after this
/// helper returns `Applied`; the intermediate checkpoint must therefore be non-queued.
pub async fn complete_controller_wake_in_conn(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    controller_generation: u64,
    wake_epoch: u64,
    checkpoint: ExecutionRunActivationCheckpoint,
) -> Result<RunControllerCompletionOutcome> {
    validate_activation_checkpoint(&checkpoint)?;
    if checkpoint.activation_state == ExecutionActivationState::Queued {
        return Err(Error::InvalidRepositoryInput {
            message: "transactional controller checkpoint must enqueue after acknowledging"
                .to_string(),
        });
    }
    let generation = to_i64(controller_generation, "controller generation")?;
    let wake = to_i64(wake_epoch, "wake epoch")?;
    let ready = to_i64(checkpoint.ready_task_count, "ready task count")?;
    let active = to_i64(checkpoint.active_task_count, "active task count")?;
    let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(RunControllerCompletionOutcome::NotFound);
    };
    let run = run_from_row(&row)?;
    if run.controller_generation != controller_generation {
        return Ok(RunControllerCompletionOutcome::StaleGeneration {
            current_generation: run.controller_generation,
        });
    }
    if wake_epoch <= run.processed_wake_epoch {
        return Ok(RunControllerCompletionOutcome::Replayed(Box::new(run)));
    }
    if run.wake_epoch != wake_epoch {
        return Ok(RunControllerCompletionOutcome::StaleWake {
            current_wake_epoch: run.wake_epoch,
            processed_wake_epoch: run.processed_wake_epoch,
        });
    }
    if !matches!(
        run.activation_state,
        ExecutionActivationState::Advancing | ExecutionActivationState::Queued
    ) {
        return Ok(RunControllerCompletionOutcome::InvalidState);
    }
    let updated = sqlx::query(
        "UPDATE moa.execution_run SET status = $4, activation_state = $5, \
             next_wake_at = $6, waiting_since = $7, ready_task_count = $8, \
             active_task_count = $9, processed_wake_epoch = $3, \
             activation_failure_count = 0, updated_at = NOW() \
         WHERE run_uid = $1 AND controller_generation = $2 \
           AND wake_epoch = $3 AND processed_wake_epoch < $3 RETURNING *",
    )
    .bind(run_uid)
    .bind(generation)
    .bind(wake)
    .bind(checkpoint.status.as_str())
    .bind(checkpoint.activation_state.as_str())
    .bind(checkpoint.next_wake_at)
    .bind(checkpoint.waiting_since)
    .bind(ready)
    .bind(active)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::Storage {
        message: "transactional controller completion lost its wake fence".to_string(),
    })?;
    Ok(RunControllerCompletionOutcome::Applied {
        run: Box::new(run_from_row(&updated)?),
        continuation: None,
    })
}

/// Records consecutive crashed controller activations inside the caller's transaction.
///
/// [`complete_controller_wake_in_conn`] resets this counter for every acknowledged wake, so the
/// resumed-recovery path must restore its incremented value after acknowledging the crashed wake.
async fn record_activation_failure_count_in_conn(
    conn: &mut PgConnection,
    run_uid: Uuid,
    consecutive_failures: u64,
) -> Result<()> {
    sqlx::query(
        "UPDATE moa.execution_run SET activation_failure_count = $2, updated_at = NOW() \
         WHERE run_uid = $1",
    )
    .bind(run_uid)
    .bind(to_i64(consecutive_failures, "activation failure count")?)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

fn run_activation_dispatch_uid(run_uid: Uuid, controller_generation: u64, wake_epoch: u64) -> Uuid {
    let name = format!("{run_uid}:{controller_generation}:{wake_epoch}");
    Uuid::new_v5(&RUN_ACTIVATION_DISPATCH_NAMESPACE, name.as_bytes())
}

fn run_deadline_trigger_uid(
    run_uid: Uuid,
    controller_generation: u64,
    deadline_at: DateTime<Utc>,
) -> Uuid {
    let name = format!(
        "{run_uid}:{controller_generation}:{}",
        deadline_at.timestamp_micros()
    );
    Uuid::new_v5(&RUN_DEADLINE_TRIGGER_NAMESPACE, name.as_bytes())
}

fn initial_activation_state(status: ExecutionRunStatus) -> Result<ExecutionActivationState> {
    match status {
        ExecutionRunStatus::AwaitingConfirmation => Ok(ExecutionActivationState::Idle),
        ExecutionRunStatus::Queued => Ok(ExecutionActivationState::Queued),
        _ => Err(Error::InvalidRepositoryInput {
            message: "new execution run must be awaiting confirmation or queued".to_string(),
        }),
    }
}

fn validate_activation_checkpoint(checkpoint: &ExecutionRunActivationCheckpoint) -> Result<()> {
    if checkpoint.activation_state == ExecutionActivationState::Advancing {
        return Err(Error::InvalidRepositoryInput {
            message: "a completed controller activation cannot checkpoint as advancing".to_string(),
        });
    }
    if checkpoint.status.is_terminal()
        != (checkpoint.activation_state == ExecutionActivationState::Terminal)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "terminal run status and terminal activation state must agree".to_string(),
        });
    }
    if checkpoint.activation_state == ExecutionActivationState::Terminal
        && (checkpoint.next_wake_at.is_some()
            || checkpoint.waiting_since.is_some()
            || checkpoint.ready_task_count != 0
            || checkpoint.active_task_count != 0)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "terminal activation checkpoints cannot retain wakes, waits, or task counts"
                .to_string(),
        });
    }
    if checkpoint.status == ExecutionRunStatus::Paused
        && (checkpoint.activation_state != ExecutionActivationState::Paused
            || checkpoint.active_task_count != 0)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "paused runs must use paused activation state with zero active tasks"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_controller_completion(request: &RunControllerCompletionRequest) -> Result<()> {
    let continuation = request.continuation_payload.is_some();
    if continuation != (request.checkpoint.activation_state == ExecutionActivationState::Queued) {
        return Err(Error::InvalidRepositoryInput {
            message: "queued controller checkpoint requires exactly one continuation payload"
                .to_string(),
        });
    }
    if request
        .continuation_payload
        .as_ref()
        .is_some_and(|payload| !payload.is_object())
    {
        return Err(Error::InvalidRepositoryInput {
            message: "controller continuation payload must be a JSON object".to_string(),
        });
    }
    Ok(())
}

fn activation_checkpoint_matches(
    run: &ExecutionRunRecord,
    checkpoint: &ExecutionRunActivationCheckpoint,
) -> bool {
    run.status == checkpoint.status
        && run.activation_state == checkpoint.activation_state
        && run.next_wake_at == checkpoint.next_wake_at
        && run.waiting_since == checkpoint.waiting_since
        && run.ready_task_count == checkpoint.ready_task_count
        && run.active_task_count == checkpoint.active_task_count
}

fn requires_parked_run_capacity(run: &ExecutionRunRecord) -> bool {
    requires_parked_capacity_checkpoint(
        run.status,
        run.activation_state,
        run.ready_task_count,
        run.active_task_count,
    )
}

fn requires_parked_capacity_checkpoint(
    status: ExecutionRunStatus,
    activation_state: ExecutionActivationState,
    ready_task_count: u64,
    active_task_count: u64,
) -> bool {
    activation_state == ExecutionActivationState::Idle
        && ready_task_count == 0
        && active_task_count == 0
        && matches!(
            status,
            ExecutionRunStatus::WaitingInput
                | ExecutionRunStatus::WaitingReview
                | ExecutionRunStatus::WaitingSignal
                | ExecutionRunStatus::WaitingTimer
                | ExecutionRunStatus::WaitingExternal
                | ExecutionRunStatus::WaitingReplan
        )
}

pub(super) fn active_run_capacity_request(
    tenant_id: TenantId,
    run_uid: Uuid,
) -> ExecutionCapacityRequest {
    ExecutionCapacityRequest {
        reservation_uid: execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ActiveRuns,
            run_uid,
            None,
        ),
        tenant_id,
        run_uid: Some(run_uid),
        controller_generation: Some(RUN_LIFETIME_CAPACITY_GENERATION),
        dimension: ExecutionCapacityDimension::ActiveRuns,
        owner: ExecutionCapacityOwner::Run,
        expires_at: None,
    }
}

/// Set-seeds tenant dispatch state and every canonical node inside run admission.
pub(super) async fn seed_run_scheduler_state_in_tx(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    run_uid: Uuid,
    plan: &CanonicalExecutionPlan,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.execution_tenant_dispatch_state (tenant_id) VALUES ($1) \
         ON CONFLICT (tenant_id) DO NOTHING",
    )
    .bind(tenant_id.0)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;

    if plan.definition.nodes.is_empty() {
        return Ok(());
    }
    let mut node_state_uids = Vec::with_capacity(plan.definition.nodes.len());
    let mut node_ids = Vec::with_capacity(plan.definition.nodes.len());
    let mut node_orders = Vec::with_capacity(plan.definition.nodes.len());
    let mut dependency_counts = Vec::with_capacity(plan.definition.nodes.len());
    for (node_order, node) in plan.definition.nodes.iter().enumerate() {
        node_state_uids.push(Uuid::new_v5(&run_uid, node.id.as_bytes()));
        node_ids.push(node.id.clone());
        node_orders.push(
            i64::try_from(node_order).map_err(|_| Error::InvalidRepositoryInput {
                message: "execution node order exceeds PostgreSQL BIGINT".to_string(),
            })?,
        );
        dependency_counts.push(i64::try_from(node.depends_on.len()).map_err(|_| {
            Error::InvalidRepositoryInput {
                message: "execution dependency count exceeds PostgreSQL BIGINT".to_string(),
            }
        })?);
    }
    let inserted = sqlx::query(
        "INSERT INTO moa.execution_node_state (\
             node_state_uid, tenant_id, run_uid, node_id, node_order, \
             dependency_count, remaining_dependency_count\
         ) \
         SELECT seed.node_state_uid, $1, $2, seed.node_id, seed.node_order, \
                seed.dependency_count, seed.dependency_count \
         FROM UNNEST($3::UUID[], $4::TEXT[], $5::BIGINT[], $6::BIGINT[]) AS seed(\
             node_state_uid, node_id, node_order, dependency_count\
         )",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .bind(node_state_uids)
    .bind(node_ids)
    .bind(node_orders)
    .bind(dependency_counts)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let expected =
        u64::try_from(plan.definition.nodes.len()).map_err(|_| Error::InvalidRepositoryInput {
            message: "execution node count exceeds PostgreSQL BIGINT".to_string(),
        })?;
    if inserted.rows_affected() != expected {
        return Err(Error::InvalidRepositoryData {
            message: "execution run admission did not seed every canonical plan node".to_string(),
        });
    }
    Ok(())
}

pub(super) fn validate_new_run(scope: ExecutionScope, new_run: &NewExecutionRun) -> Result<()> {
    if new_run
        .contact_id
        .is_some_and(|contact_id| contact_id.0.is_nil())
    {
        return Err(Error::InvalidRepositoryInput {
            message: "execution run contact_id must not be nil".to_string(),
        });
    }
    if !scope.permits_owner(new_run.tenant_id, new_run.contact_id) {
        return Err(Error::InvalidRepositoryInput {
            message: "run owner does not match the repository scope".to_string(),
        });
    }
    if new_run.admitted_identity.tenant_id != new_run.tenant_id {
        return Err(Error::InvalidRepositoryInput {
            message: "admitted identity tenant does not match the execution run tenant".to_string(),
        });
    }
    if !matches!(
        new_run.status,
        ExecutionRunStatus::AwaitingConfirmation | ExecutionRunStatus::Queued
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "new runs must start awaiting_confirmation or queued".to_string(),
        });
    }
    if new_run.plan.estimate.tasks == 0 {
        return Err(Error::InvalidRepositoryInput {
            message: "a canonical run plan must estimate at least one logical task".to_string(),
        });
    }
    if new_run.catalog.catalog_hash != new_run.plan.catalog_hash {
        return Err(Error::InvalidRepositoryInput {
            message: "persisted catalog hash does not match the canonical plan".to_string(),
        });
    }
    new_run
        .source_provenance
        .validate(&new_run.plan.plan_hash.to_string())
        .map_err(|error| Error::InvalidRepositoryInput {
            message: format!("invalid execution source provenance: {error}"),
        })?;
    let mut pinned = new_run.pinned_instruction_skills.clone();
    pinned.sort_by(|left, right| {
        left.skill_ref
            .to_string()
            .cmp(&right.skill_ref.to_string())
            .then_with(|| left.revision_uid.cmp(&right.revision_uid))
    });
    if pinned != new_run.pinned_instruction_skills
        || pinned.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be sorted and duplicate-free".to_string(),
        });
    }
    if new_run
        .pinned_instruction_skills
        .iter()
        .any(|pinned| !new_run.authorization.skill_refs.contains(&pinned.skill_ref))
    {
        return Err(Error::InvalidRepositoryInput {
            message: "pinned instruction skills must be present in the authorization envelope"
                .to_string(),
        });
    }
    Ok(())
}

pub(super) struct NormalizedSourceFields<'a> {
    kind: ExecutionSourceKind,
    skill_template_ref: Option<&'a str>,
    skill_template_revision_uid: Option<Uuid>,
}

pub(super) fn normalized_source_fields(
    provenance: &ExecutionSourceProvenance,
) -> NormalizedSourceFields<'_> {
    match provenance {
        ExecutionSourceProvenance::GeneratedPlan { .. } => NormalizedSourceFields {
            kind: ExecutionSourceKind::GeneratedPlan,
            skill_template_ref: None,
            skill_template_revision_uid: None,
        },
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::SkillTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
        ExecutionSourceProvenance::ExperimentTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => NormalizedSourceFields {
            kind: ExecutionSourceKind::ExperimentTemplate,
            skill_template_ref: Some(skill_template_ref.as_str()),
            skill_template_revision_uid: Some(*skill_template_revision_uid),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn continuation_dispatch_identity_is_fenced_by_generation_and_wake() {
        // Pins: retrying the same completion addresses one outbox row, while a later wake or
        // controller generation can never alias that durable continuation.
        let run_uid = Uuid::from_u128(0x11);
        let first = run_activation_dispatch_uid(run_uid, 7, 9);

        assert_eq!(first, run_activation_dispatch_uid(run_uid, 7, 9));
        assert_ne!(first, run_activation_dispatch_uid(run_uid, 7, 10));
        assert_ne!(first, run_activation_dispatch_uid(run_uid, 8, 9));
    }

    #[test]
    fn run_deadline_trigger_identity_changes_only_with_its_generation_or_deadline() {
        // Pins: an amended deadline replaces its old immutable trigger while an exact replay
        // resolves to the same trigger tombstone.
        let run_uid = Uuid::from_u128(0x22);
        let deadline = Utc
            .timestamp_opt(1_800_000_000, 0)
            .single()
            .expect("test timestamp is representable");
        let first = run_deadline_trigger_uid(run_uid, 4, deadline);

        assert_eq!(first, run_deadline_trigger_uid(run_uid, 4, deadline));
        assert_ne!(
            first,
            run_deadline_trigger_uid(run_uid, 4, deadline + chrono::TimeDelta::seconds(1))
        );
        assert_ne!(first, run_deadline_trigger_uid(run_uid, 5, deadline));
    }

    #[test]
    fn queued_checkpoint_requires_exactly_one_continuation_payload() {
        // Pins: a queued checkpoint cannot commit without its same-transaction outbox row.
        let request = RunControllerCompletionRequest {
            controller_generation: 1,
            wake_epoch: 2,
            checkpoint: ExecutionRunActivationCheckpoint {
                status: ExecutionRunStatus::Running,
                activation_state: ExecutionActivationState::Queued,
                next_wake_at: None,
                waiting_since: None,
                ready_task_count: 0,
                active_task_count: 0,
            },
            continuation_payload: None,
            continuation_not_before_at: Utc::now(),
        };

        let error = validate_controller_completion(&request)
            .expect_err("queued checkpoint without outbox payload must fail");
        assert_eq!(
            error.to_string(),
            "invalid execution repository request: queued controller checkpoint requires exactly one continuation payload"
        );
    }

    #[test]
    fn only_zero_compute_storage_waits_reserve_parked_run_capacity() {
        // Pins: parked capacity is acquired exactly when the controller can return with no ready
        // or active work; queued/running work cannot double-count as a parked run.
        assert!(requires_parked_capacity_checkpoint(
            ExecutionRunStatus::WaitingTimer,
            ExecutionActivationState::Idle,
            0,
            0,
        ));
        assert!(!requires_parked_capacity_checkpoint(
            ExecutionRunStatus::WaitingTimer,
            ExecutionActivationState::Idle,
            0,
            1,
        ));
        assert!(!requires_parked_capacity_checkpoint(
            ExecutionRunStatus::Running,
            ExecutionActivationState::Idle,
            0,
            0,
        ));
        assert!(!requires_parked_capacity_checkpoint(
            ExecutionRunStatus::WaitingTimer,
            ExecutionActivationState::Queued,
            0,
            0,
        ));
    }
}
