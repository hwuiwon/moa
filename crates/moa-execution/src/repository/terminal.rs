//! Successful run finalization and shared state-projection helpers.

use super::*;

/// Exact amendment identity that caused a compensation-safe replan-stop fence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplanStopReceipt {
    /// Waiting-replan task whose current outcome triggered amendment evaluation.
    pub task_id: ExecutionTaskId,
    /// Exact generation of the waiting-replan task.
    pub task_generation: u64,
    /// Plan revision against which the amendment was evaluated.
    pub base_plan_revision: u64,
    /// Domain-separated hash of the exact amendment request.
    pub amendment_hash: ExecutionHash,
}

/// Durable phase reached by one bounded pending-terminal advancement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingTerminalAdvanceStage {
    /// A bounded page settled or claimed work and another controller activation was queued.
    EnqueuedPage,
    /// Exact active attempts are relinquishing hands and still own capacity.
    Draining,
    /// Forward work drained and bounded reverse-order compensation was activated.
    CompensationQueued,
    /// The held terminal intent became the run's final state.
    Finalized,
    /// Compensation evidence requires governed repair instead of a clean terminal result.
    ManualRepairRequired,
}

/// Durable receipts committed by one bounded pending-terminal advancement.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingTerminalAdvanceCommit {
    /// Current run after the exact wake checkpoint was committed.
    pub run: ExecutionRunRecord,
    /// Lifecycle phase reached by this bounded page.
    pub stage: PendingTerminalAdvanceStage,
    /// Number of storage-only forward tasks settled by this page.
    pub settled_task_count: u64,
    /// Number of durable triggers superseded within the same bounded page.
    pub drained_trigger_count: u32,
    /// Exact task or compensation cancellation deliveries enqueued by this page.
    pub cancellation_dispatches: Vec<ExecutionDispatchRecord>,
    /// At most one reverse-order compensation slice admitted by this activation.
    pub compensation_admission: Option<Box<compensation::CompensationAttemptAdmission>>,
    /// Optional controller continuation created in the same transaction.
    pub continuation: Option<Box<ExecutionDispatchRecord>>,
    /// Whether durable work still prevents installing the held terminal result.
    pub work_remaining: bool,
}

/// Generation-and-wake-fenced result of advancing a bounded terminal drain page.
#[derive(Clone, Debug, PartialEq)]
pub enum PendingTerminalAdvanceOutcome {
    /// This invocation committed one bounded monotonic page.
    Applied(Box<PendingTerminalAdvanceCommit>),
    /// The exact wake was already acknowledged and no delivery was duplicated.
    Replayed(Box<PendingTerminalAdvanceCommit>),
    /// No visible run exists under the supplied scope.
    NotFound,
    /// Generation, wake, terminal intent, or lifecycle state did not match.
    Conflict,
}

/// Result of terminal run finalization.
#[derive(Clone, Debug, PartialEq)]
pub enum FinalizationOutcome {
    /// Terminal state and completion evidence were persisted.
    Finalized(ExecutionRunRecord),
    /// The same terminal projection was already persisted.
    Replayed(ExecutionRunRecord),
    /// No visible run exists.
    NotFound,
    /// Revision, status, or completion evaluation did not match.
    Conflict,
}

/// Durable receipt for one bounded page of active run-trigger settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct RunTriggerDrainCommit {
    /// Run after the exact wake was acknowledged and its continuation was queued.
    pub run: ExecutionRunRecord,
    /// Exact number of triggers superseded by this bounded page.
    pub drained_trigger_count: u32,
    /// Controller continuation committed in the same transaction.
    pub continuation: Box<ExecutionDispatchRecord>,
}

/// Exact controller fence and page bound for one terminal trigger-drain activation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunTriggerDrainRequest {
    /// Run whose remaining durable triggers must be settled.
    pub run_uid: Uuid,
    /// Exact controller generation that owns this drain activation.
    pub controller_generation: u64,
    /// Exact unprocessed wake epoch claimed by the controller.
    pub wake_epoch: u64,
    /// Maximum number of triggers settled by this bounded page.
    pub page_limit: u32,
    /// Database-operation timestamp shared by the page transaction.
    pub now: DateTime<Utc>,
}

/// Generation-and-wake-fenced result of draining active run triggers before finalization.
#[derive(Clone, Debug, PartialEq)]
pub enum RunTriggerDrainOutcome {
    /// One bounded page committed and another activation owns the remaining work.
    PageDrained(Box<RunTriggerDrainCommit>),
    /// No active trigger or trigger delivery remains; the same activation may finalize.
    ReadyToFinalize {
        /// Current locked run projection.
        run: Box<ExecutionRunRecord>,
        /// Exact number of triggers settled by this final page.
        drained_trigger_count: u32,
    },
    /// The exact wake was already acknowledged; its durable continuation must not be duplicated.
    Replayed(Box<ExecutionRunRecord>),
    /// No visible run exists under the supplied scope.
    NotFound,
    /// The supplied controller generation is stale.
    StaleGeneration {
        /// Current persisted generation.
        current_generation: u64,
    },
    /// The supplied wake is not the current unprocessed wake.
    StaleWake {
        /// Current persisted wake epoch.
        current_wake_epoch: u64,
        /// Greatest wake epoch already acknowledged by the controller.
        processed_wake_epoch: u64,
    },
    /// The run lifecycle cannot accept trigger drain work.
    InvalidState,
}

/// Optimistically fenced request to atomically persist one terminal run projection.
#[derive(Clone, Debug, PartialEq)]
pub struct RunFinalizationRequest {
    /// Run to finalize.
    pub run_uid: Uuid,
    /// Active plan revision used for completion evaluation.
    pub expected_revision: u64,
    /// Wake epoch of the structured projection used for completion evaluation.
    pub expected_wake_epoch: u64,
    /// Exact terminal projection selected by the scheduler.
    pub terminal_projection: TerminalProjection,
    /// Deterministic completion evaluation over the observed projection.
    pub completion_evaluation: CompletionEvaluation,
    /// Exact typed cause and requirement-count replay identity.
    pub terminal_evidence: ExecutionTerminalEvidence,
    /// Exact normalized terminal reason selected from typed evidence.
    pub terminal_reason: ExecutionTerminalReason,
}
use super::{
    capacity::{
        ExecutionCapacityDimension, prelock_existing_capacity_dimensions_in_tx,
        release_owned_run_capacity_in_tx,
    },
    projection::*,
    rows::*,
    run::{complete_controller_wake_in_conn, enqueue_run_activation_in_conn},
    sql::*,
    trigger::{ExecutionTriggerKind, ExecutionTriggerSupersedeOutcome, supersede_trigger_in_conn},
};

const MAX_TRIGGER_DRAIN_PAGE_SIZE: u32 = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RunTriggerDrainPage {
    pub(super) drained_trigger_count: u32,
    pub(super) work_remaining: bool,
    pub(super) next_wake_at: Option<DateTime<Utc>>,
}

impl ExecutionRepository {
    /// Settles one bounded page of run triggers before terminal finalization.
    pub async fn drain_run_triggers_page(
        &self,
        scope: ExecutionScope,
        config: &moa_config::ExecutionConfig,
        request: RunTriggerDrainRequest,
    ) -> Result<RunTriggerDrainOutcome> {
        let RunTriggerDrainRequest {
            run_uid,
            controller_generation,
            wake_epoch,
            page_limit,
            now,
        } = request;
        validate_trigger_drain_page_limit(page_limit)?;
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
            return Ok(RunTriggerDrainOutcome::NotFound);
        };
        super::capacity::prelock_capacity_dimensions_in_tx(
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
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::NotFound);
        };
        let run = run_from_row(&row)?;
        if run.controller_generation != controller_generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::StaleGeneration {
                current_generation: run.controller_generation,
            });
        }
        if wake_epoch <= run.processed_wake_epoch {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::Replayed(Box::new(run)));
        }
        if run.wake_epoch != wake_epoch {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::StaleWake {
                current_wake_epoch: run.wake_epoch,
                processed_wake_epoch: run.processed_wake_epoch,
            });
        }
        if run.status.is_terminal() || run.activation_state != ExecutionActivationState::Advancing {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::InvalidState);
        }

        let page = drain_run_triggers_page_in_conn(&mut conn, &run, page_limit).await?;
        if !page.work_remaining {
            conn.commit().await.map_err(storage_error)?;
            return Ok(RunTriggerDrainOutcome::ReadyToFinalize {
                run: Box::new(run),
                drained_trigger_count: page.drained_trigger_count,
            });
        }

        let checkpoint = ExecutionRunActivationCheckpoint {
            status: run.status,
            activation_state: ExecutionActivationState::Idle,
            next_wake_at: page.next_wake_at,
            waiting_since: run.waiting_since,
            ready_task_count: run.ready_task_count,
            active_task_count: run.active_task_count,
        };
        let checkpointed = match complete_controller_wake_in_conn(
            &mut conn,
            run_uid,
            controller_generation,
            wake_epoch,
            checkpoint,
        )
        .await?
        {
            RunControllerCompletionOutcome::Applied { run, .. } => run,
            RunControllerCompletionOutcome::Replayed(run) => {
                conn.commit().await.map_err(storage_error)?;
                return Ok(RunTriggerDrainOutcome::Replayed(run));
            }
            RunControllerCompletionOutcome::NotFound => {
                conn.rollback().await.map_err(storage_error)?;
                return Err(Error::InvalidRepositoryData {
                    message: "row-locked run disappeared during trigger-drain checkpoint"
                        .to_string(),
                });
            }
            RunControllerCompletionOutcome::StaleGeneration { current_generation } => {
                conn.commit().await.map_err(storage_error)?;
                return Ok(RunTriggerDrainOutcome::StaleGeneration { current_generation });
            }
            RunControllerCompletionOutcome::StaleWake {
                current_wake_epoch,
                processed_wake_epoch,
            } => {
                conn.commit().await.map_err(storage_error)?;
                return Ok(RunTriggerDrainOutcome::StaleWake {
                    current_wake_epoch,
                    processed_wake_epoch,
                });
            }
            RunControllerCompletionOutcome::InvalidState
            | RunControllerCompletionOutcome::CapacitySaturated { .. } => {
                conn.rollback().await.map_err(storage_error)?;
                return Err(Error::InvalidRepositoryData {
                    message: "trigger-drain checkpoint rejected a locked advancing run".to_string(),
                });
            }
        };
        let continuation = enqueue_run_activation_in_conn(
            conn.as_mut(),
            checkpointed.tenant_id,
            checkpointed.run_uid,
            checkpointed.controller_generation,
            now,
            json!({"reason": "terminal_trigger_drain"}),
        )
        .await?;
        let row = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let checkpointed = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(RunTriggerDrainOutcome::PageDrained(Box::new(
            RunTriggerDrainCommit {
                run: checkpointed,
                drained_trigger_count: page.drained_trigger_count,
                continuation: Box::new(continuation),
            },
        )))
    }

    /// Atomically finalizes one successfully completed revision with deterministic evidence.
    pub async fn finalize_run(
        &self,
        scope: ExecutionScope,
        request: RunFinalizationRequest,
    ) -> Result<FinalizationOutcome> {
        let RunFinalizationRequest {
            run_uid,
            expected_revision,
            expected_wake_epoch,
            terminal_projection,
            completion_evaluation,
            terminal_evidence,
            terminal_reason,
        } = request;
        let expected_status = run_status_from_completion(completion_evaluation.status);
        if expected_status != ExecutionRunStatus::Completed {
            return Err(Error::InvalidRepositoryInput {
                message: "ordinary finalization only accepts successful completion; non-success terminal intents must use the compensation fence"
                    .to_string(),
            });
        }
        if run_status_from_terminal_projection(&terminal_projection) != expected_status {
            return Err(Error::InvalidRepositoryInput {
                message: "terminal projection and completion evaluation disagree".to_string(),
            });
        }
        let selected_reason = execution_terminal_reason(
            &terminal_evidence.cause,
            &terminal_projection,
            &completion_evaluation,
        )?;
        if terminal_reason != selected_reason {
            return Err(Error::InvalidRepositoryInput {
                message: "selected terminal reason disagrees with typed terminal evidence"
                    .to_string(),
            });
        }
        let output = terminal_projection_output(&terminal_projection);
        let checks = serde_json::to_value(&completion_evaluation.checks)?;
        let gaps = serde_json::to_value(&completion_evaluation.gaps)?;
        let terminal_cause = serde_json::to_value(&terminal_evidence.cause)?;
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
            return Ok(FinalizationOutcome::NotFound);
        };
        let capacity_labels = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT resource_dimension \
             FROM moa.execution_capacity_reservation \
             WHERE run_uid = $1 AND state IN ('reserved', 'reconciling') \
               AND resource_dimension IN ('active_runs', 'parked_runs', 'scheduled_triggers')",
        )
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let mut capacity_dimensions = Vec::with_capacity(capacity_labels.len());
        for label in capacity_labels {
            capacity_dimensions.push(match label.as_str() {
                "active_runs" => ExecutionCapacityDimension::ActiveRuns,
                "parked_runs" => ExecutionCapacityDimension::ParkedRuns,
                "scheduled_triggers" => ExecutionCapacityDimension::ScheduledTriggers,
                _ => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Err(Error::InvalidRepositoryData {
                        message: format!(
                            "terminal capacity prelock found unexpected dimension `{label}`"
                        ),
                    });
                }
            });
        }
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            TenantId(tenant_id),
            &capacity_dimensions,
        )
        .await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.status.is_terminal() {
            let replay = current.plan_revision == expected_revision
                && current.status == expected_status
                && current.output == output
                && serde_json::to_value(&current.completion_check_results)? == checks
                && serde_json::to_value(&current.terminal_gaps)? == gaps
                && current.terminal_evidence.as_ref() == Some(&terminal_evidence)
                && current.terminal_reason == Some(terminal_reason);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                FinalizationOutcome::Replayed(current)
            } else {
                FinalizationOutcome::Conflict
            });
        }
        if current.plan_revision != expected_revision || current.wake_epoch != expected_wake_epoch {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let terminal_boundary = sqlx::query(
            "SELECT \
               EXISTS (SELECT 1 FROM moa.execution_task \
                 WHERE run_uid = $1 AND status NOT IN \
                   ('completed', 'skipped', 'failed', 'cancelled', 'unknown_outcome')) \
                 AS has_nonterminal_tasks, \
               EXISTS (SELECT 1 FROM moa.execution_node_state \
                 WHERE run_uid = $1 AND node_status NOT IN \
                   ('completed', 'skipped', 'failed', 'cancelled')) \
                 AS has_unfinished_nodes",
        )
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let has_nonterminal_tasks: bool = terminal_boundary
            .try_get("has_nonterminal_tasks")
            .map_err(row_error)?;
        let has_unfinished_nodes: bool = terminal_boundary
            .try_get("has_unfinished_nodes")
            .map_err(row_error)?;
        if current.pending_terminal.is_some()
            || current.manual_repair_required
            || has_nonterminal_tasks
            || has_unfinished_nodes
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let expected_terminal_evidence = terminal_evidence_from_evaluation(
            terminal_evidence.cause.clone(),
            &completion_evaluation,
        )?;
        if terminal_evidence != expected_terminal_evidence {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let has_pending_trigger_work: bool = sqlx::query_scalar(
            "SELECT \
               EXISTS (SELECT 1 FROM moa.execution_trigger \
                 WHERE run_uid = $1 AND state = 'pending') \
               OR EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox \
                 WHERE run_uid = $1 AND trigger_uid IS NOT NULL \
                   AND state IN ('pending', 'dispatching')) \
               OR EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
                 WHERE run_uid = $1 AND resource_dimension = 'scheduled_triggers' \
                   AND state IN ('reserved', 'reconciling'))",
        )
        .bind(current.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if has_pending_trigger_work {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        release_owned_run_capacity_in_tx(
            conn.as_mut(),
            current.tenant_id,
            current.run_uid,
            current.controller_generation,
        )
        .await?;
        let row = sqlx::query(
            "UPDATE moa.execution_run \
             SET status = $3, output = $4, completion_check_results = $5, \
                 terminal_gaps = $6, terminal_cause = $7, \
                 terminal_satisfied_requirement_count = $8, \
                 terminal_requirement_count = $9, terminal_reason = $10, \
                 reserved_cost_microusd = 0, reserved_tokens = 0, \
                 reserved_tasks = 0, reserved_tool_calls = 0, \
                 reserved_retrieved_bytes = 0, \
                 next_wake_at = NULL, waiting_since = NULL, \
                 waiting_reasons = '[]'::JSONB, waiting_task_count = 0, \
                 waiting_input_task_count = 0, waiting_review_task_count = 0, \
                 waiting_signal_task_count = 0, waiting_timer_task_count = 0, \
                 waiting_external_task_count = 0, waiting_replan_task_count = 0, \
                 waiting_input_user_task_count = 0, \
                 waiting_input_tenant_admin_task_count = 0, \
                 waiting_input_external_task_count = 0, \
                 waiting_reasons_truncated = FALSE, \
                 processed_wake_epoch = $11, wake_epoch = wake_epoch + 1, \
                 activation_state = 'terminal', completed_at = NOW(), updated_at = NOW() \
             WHERE run_uid = $1 AND plan_revision = $2 \
             RETURNING *",
        )
        .bind(run_uid)
        .bind(to_i64(expected_revision, "expected plan revision")?)
        .bind(expected_status.as_str())
        .bind(output)
        .bind(checks)
        .bind(gaps)
        .bind(terminal_cause)
        .bind(to_i64(
            terminal_evidence.satisfied_requirement_count,
            "terminal satisfied requirement count",
        )?)
        .bind(to_i64(
            terminal_evidence.requirement_count,
            "terminal requirement count",
        )?)
        .bind(terminal_reason.as_str())
        .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(FinalizationOutcome::Finalized(run))
    }
}

/// Supersedes one stable bounded page of active run triggers in the caller transaction.
///
/// This is the single trigger/outbox/capacity settlement path shared by successful and held
/// terminal flows. Callers own the run row lock and must checkpoint or finalize before commit.
pub(super) async fn drain_run_triggers_page_in_conn(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    page_limit: u32,
) -> Result<RunTriggerDrainPage> {
    validate_trigger_drain_page_limit(page_limit)?;
    let trigger_rows = sqlx::query(
        "SELECT trigger_uid, trigger_kind, controller_generation, attempt_generation, \
                compensation_generation, compensation_attempt_generation \
         FROM moa.execution_trigger \
         WHERE run_uid = $1 AND state = 'pending' \
         ORDER BY trigger_kind, trigger_uid LIMIT $2 FOR UPDATE",
    )
    .bind(run.run_uid)
    .bind(i64::from(page_limit))
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let drained_trigger_count =
        u32::try_from(trigger_rows.len()).map_err(|_| Error::InvalidRepositoryData {
            message: "bounded trigger-drain page count exceeds u32".to_string(),
        })?;
    for trigger in trigger_rows {
        let trigger_uid = trigger
            .try_get::<Uuid, _>("trigger_uid")
            .map_err(row_error)?;
        let trigger_kind = trigger
            .try_get::<String, _>("trigger_kind")
            .map_err(row_error)?
            .parse::<ExecutionTriggerKind>()?;
        match supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            trigger_kind,
            optional_u64(&trigger, "controller_generation")?,
            optional_u64(&trigger, "attempt_generation")?,
            optional_u64(&trigger, "compensation_generation")?,
            optional_u64(&trigger, "compensation_attempt_generation")?,
        )
        .await?
        {
            ExecutionTriggerSupersedeOutcome::Superseded
            | ExecutionTriggerSupersedeOutcome::AlreadySuperseded
            | ExecutionTriggerSupersedeOutcome::AlreadyInactive => {}
            ExecutionTriggerSupersedeOutcome::StaleOrMissing => {
                return Err(Error::InvalidRepositoryData {
                    message: "row-locked terminal trigger disappeared during bounded drain"
                        .to_string(),
                });
            }
        }
    }
    let next_wake_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT MIN(due_at) FROM moa.execution_trigger \
         WHERE run_uid = $1 AND state = 'pending'",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(RunTriggerDrainPage {
        drained_trigger_count,
        work_remaining: next_wake_at.is_some(),
        next_wake_at,
    })
}

fn validate_trigger_drain_page_limit(page_limit: u32) -> Result<()> {
    if page_limit == 0 || page_limit > MAX_TRIGGER_DRAIN_PAGE_SIZE {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "trigger-drain page limit must be between 1 and {MAX_TRIGGER_DRAIN_PAGE_SIZE}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_repository_exposes_no_direct_non_success_finalizers() {
        // Pins: every non-success terminal intent must enter through the
        // compensation fence; restoring either legacy transaction bypasses
        // forward-task join and rollback settlement.
        let source = include_str!("terminal.rs");
        let direct_cancel = ["pub async fn ", "cancel_run("].concat();
        let direct_replan = ["pub async fn ", "finalize_replan_stop("].concat();

        assert!(
            !source.contains(&direct_cancel),
            "legacy direct cancellation remains publicly dispatchable"
        );
        assert!(
            !source.contains(&direct_replan),
            "legacy direct replan-stop finalization remains publicly dispatchable"
        );
    }
}
