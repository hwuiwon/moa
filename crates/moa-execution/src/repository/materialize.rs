//! Run confirmation and idempotent logical-task materialization.

use super::*;
use super::{projection::budget_ledger, rows::*, sql::*};

impl ExecutionRepository {
    /// Confirms the exact displayed active-plan hash and atomically persists its budget.
    pub async fn confirm_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_plan_hash: &ExecutionHash,
        approved_budget: ExecutionBudgetLimit,
    ) -> Result<ConfirmationOutcome> {
        let budget = DbBudgetLimit::try_from(&approved_budget)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.active_plan_hash != *expected_plan_hash {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::Conflict(
                ConfirmationConflict::PlanHashMismatch,
            ));
        }
        if current.status != ExecutionRunStatus::AwaitingConfirmation {
            let outcome = if current.status.is_terminal()
                || current.confirmed_at.is_none()
                || current.confirmed_plan_hash.as_ref() != Some(expected_plan_hash)
            {
                ConfirmationOutcome::Conflict(ConfirmationConflict::InvalidStatus)
            } else if current.approved_budget == approved_budget {
                ConfirmationOutcome::AlreadyConfirmed(current)
            } else {
                ConfirmationOutcome::Conflict(ConfirmationConflict::BudgetMismatch)
            };
            conn.commit().await.map_err(storage_error)?;
            return Ok(outcome);
        }

        let row = sqlx::query(CONFIRM_RUN_SQL)
            .bind(run_uid)
            .bind(expected_plan_hash.to_string())
            .bind(budget.max_cost_microusd)
            .bind(budget.max_tokens)
            .bind(budget.max_tasks)
            .bind(budget.max_tool_calls)
            .bind(budget.max_retrieved_bytes)
            .bind(budget.deadline_at)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ConfirmationOutcome::Conflict(
                ConfirmationConflict::InvalidStatus,
            ));
        };
        let confirmed = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ConfirmationOutcome::Confirmed(confirmed))
    }

    /// Materializes stable logical tasks exactly once for the active plan revision.
    pub async fn materialize_tasks(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        plan_revision: u64,
        tasks: Vec<LogicalTask>,
    ) -> Result<Vec<ExecutionTaskRecord>> {
        match self
            .materialize_node(scope, run_uid, plan_revision, None, tasks)
            .await?
        {
            MaterializationOutcome::Applied(evidence) => Ok(evidence.tasks),
            MaterializationOutcome::Replayed { tasks } => Ok(tasks),
            MaterializationOutcome::Conflict => Err(Error::InvalidRepositoryInput {
                message: "task materialization conflicts with first persisted semantics"
                    .to_string(),
            }),
        }
    }

    /// Materializes one node and returns first-application evidence for metric emission.
    pub async fn materialize_node(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        plan_revision: u64,
        marker: Option<ExecutionNodeMaterialization>,
        tasks: Vec<LogicalTask>,
    ) -> Result<MaterializationOutcome> {
        let plan_revision_db = to_i64(plan_revision, "plan revision")?;
        if let Some(marker) = marker.as_ref()
            && tasks.iter().any(|task| task.node_id != marker.node_id())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "aggregate materialization tasks must share the marker node".to_string(),
            });
        }
        let marker_fanout_items = marker
            .as_ref()
            .and_then(ExecutionNodeMaterialization::fanout_items)
            .map(|value| to_i64(value, "map fanout items"))
            .transpose()?;
        let marker_reducer_depth = marker
            .as_ref()
            .and_then(ExecutionNodeMaterialization::reducer_depth)
            .map(|value| to_i64(value, "reducer depth"))
            .transpose()?;
        let task_batch = prepare_task_materialization_batch(run_uid, plan_revision, &tasks)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "cannot materialize tasks for a missing run".to_string(),
            });
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != plan_revision {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "task materialization plan revision is stale".to_string(),
            });
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryInput {
                message: "tasks may materialize only for queued or running runs".to_string(),
            });
        }

        let marker_applied = if let Some(marker) = marker.as_ref() {
            let inserted = sqlx::query(INSERT_NODE_MATERIALIZATION_SQL)
                .bind(run_uid)
                .bind(run.tenant_id.0)
                .bind(run.contact_id.map(|value| value.0))
                .bind(plan_revision_db)
                .bind(marker.node_id())
                .bind(marker.kind_label())
                .bind(marker_fanout_items)
                .bind(marker_reducer_depth)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            if inserted.is_none() {
                let existing = sqlx::query(LOAD_NODE_MATERIALIZATION_SQL)
                    .bind(run_uid)
                    .bind(plan_revision_db)
                    .bind(marker.node_id())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                let Some(existing) = existing else {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(MaterializationOutcome::Conflict);
                };
                let existing_kind: String = existing.try_get("kind").map_err(row_error)?;
                let existing_fanout = optional_u64(&existing, "fanout_items")?;
                let existing_depth = optional_u64(&existing, "reducer_depth")?;
                if existing_kind != marker.kind_label()
                    || existing_fanout != marker.fanout_items()
                    || existing_depth != marker.reducer_depth()
                {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(MaterializationOutcome::Conflict);
                }
                false
            } else {
                true
            }
        } else {
            false
        };

        let inserted_rows = sqlx::query(INSERT_TASK_BATCH_SQL)
            .bind(&task_batch)
            .bind(run_uid)
            .bind(run.tenant_id.0)
            .bind(run.contact_id.map(|value| value.0))
            .bind(plan_revision_db)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let mut inserted_task_ids = inserted_rows
            .iter()
            .map(|row| {
                row.try_get("task_id")
                    .map(ExecutionTaskId::from_uuid)
                    .map_err(row_error)
            })
            .collect::<Result<Vec<_>>>()?;
        let inserted_count =
            u64::try_from(inserted_task_ids.len()).map_err(|_| Error::InvalidRepositoryData {
                message: "inserted task count does not fit in u64".to_string(),
            })?;
        inserted_task_ids.sort();

        let task_rows = sqlx::query(LOAD_TASK_BATCH_SQL)
            .bind(&task_batch)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if task_rows.len() != tasks.len() {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "task materialization reloaded {} of {} requested logical keys",
                    task_rows.len(),
                    tasks.len()
                ),
            });
        }
        let records = task_rows
            .iter()
            .zip(&tasks)
            .map(|(row, requested)| {
                let existing = task_from_row(row)?;
                ensure_materialization_replay_matches(&existing, requested)?;
                Ok(existing)
            })
            .collect::<Result<Vec<_>>>()?;

        if inserted_count > 0 || marker_applied {
            sqlx::query(
                "UPDATE moa.execution_run \
                 SET progress_total_tasks = progress_total_tasks + $2, \
                     wake_epoch = wake_epoch + 1, updated_at = NOW() \
                 WHERE run_uid = $1",
            )
            .bind(run_uid)
            .bind(to_i64(inserted_count, "inserted task count")?)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        }
        conn.commit().await.map_err(storage_error)?;
        if inserted_count > 0 || marker_applied {
            Ok(MaterializationOutcome::Applied(MaterializationEvidence {
                tasks: records,
                inserted_task_ids,
                marker,
            }))
        } else {
            Ok(MaterializationOutcome::Replayed { tasks: records })
        }
    }
}

#[derive(Serialize)]
pub(super) struct TaskMaterializationRow<'a> {
    ordinal: i64,
    task_id: Uuid,
    node_id: &'a str,
    item_key: &'a str,
    requirement_ids: &'a [String],
    generation: i64,
    input: &'a Value,
    task_kind: &'a LogicalTaskKind,
    retry_policy: &'a moa_artifacts::execution_plan::RetryPolicy,
    estimate_cost_microusd: i64,
    estimate_tokens: i64,
    estimate_tasks: i64,
    estimate_tool_calls: i64,
    estimate_retrieved_bytes: i64,
    generation_history: Value,
}

pub(super) fn prepare_task_materialization_batch(
    run_uid: Uuid,
    plan_revision: u64,
    tasks: &[LogicalTask],
) -> Result<Value> {
    let mut rows = Vec::with_capacity(tasks.len());
    for (ordinal, task) in tasks.iter().enumerate() {
        validate_logical_task(run_uid, plan_revision, task)?;
        let ordinal = i64::try_from(ordinal).map_err(|_| Error::InvalidRepositoryInput {
            message: "task materialization ordinal exceeds PostgreSQL BIGINT".to_string(),
        })?;
        let estimate = DbEstimate::try_from(task.reservation)?;
        rows.push(TaskMaterializationRow {
            ordinal,
            task_id: task.task_id.as_uuid(),
            node_id: &task.node_id,
            item_key: &task.item_key,
            requirement_ids: &task.requirement_ids,
            generation: to_i64(task.generation, "task generation")?,
            input: &task.input,
            task_kind: &task.kind,
            retry_policy: &task.retry,
            estimate_cost_microusd: estimate.cost_microusd,
            estimate_tokens: estimate.tokens,
            estimate_tasks: estimate.tasks,
            estimate_tool_calls: estimate.tool_calls,
            estimate_retrieved_bytes: estimate.retrieved_bytes,
            generation_history: json!([{
                "kind": "initial",
                "attempt": 1,
                "generation": task.generation,
            }]),
        });
    }
    serde_json::to_value(rows).map_err(Into::into)
}

pub(super) fn validate_logical_task(
    run_uid: Uuid,
    plan_revision: u64,
    task: &LogicalTask,
) -> Result<()> {
    if task.plan_revision != plan_revision {
        return Err(Error::InvalidRepositoryInput {
            message: format!("task `{}` has the wrong plan revision", task.task_id),
        });
    }
    if task.generation != 1 {
        return Err(Error::InvalidRepositoryInput {
            message: format!("new task `{}` must start at generation one", task.task_id),
        });
    }
    if task.reservation.tasks != 1 {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "task `{}` must reserve exactly one logical task",
                task.task_id
            ),
        });
    }
    let expected = ExecutionTaskId::derive(run_uid, &task.node_id, &task.item_key)?;
    if expected != task.task_id {
        return Err(Error::InvalidRepositoryInput {
            message: format!("task `{}` does not match its stable identity", task.task_id),
        });
    }
    Ok(())
}

pub(super) fn ensure_materialization_replay_matches(
    existing: &ExecutionTaskRecord,
    requested: &LogicalTask,
) -> Result<()> {
    if existing.task_id != requested.task_id
        || existing.requirement_ids != requested.requirement_ids
        || existing.plan_revision != requested.plan_revision
        || existing.input != requested.input
        || existing.kind != requested.kind
        || existing.retry != requested.retry
        || existing.estimate != requested.reservation
    {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "logical task identity ({}, {}, {}) was replayed with different semantics",
                existing.run_uid, existing.node_id, existing.item_key
            ),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DbEstimate {
    pub(super) cost_microusd: i64,
    pub(super) tokens: i64,
    pub(super) tasks: i64,
    pub(super) tool_calls: i64,
    pub(super) retrieved_bytes: i64,
}

impl TryFrom<ExecutionEstimate> for DbEstimate {
    type Error = Error;

    fn try_from(value: ExecutionEstimate) -> Result<Self> {
        Ok(Self {
            cost_microusd: to_i64(value.cost_microusd, "task estimated cost")?,
            tokens: to_i64(value.tokens, "task estimated tokens")?,
            tasks: to_i64(value.tasks, "task estimated logical tasks")?,
            tool_calls: to_i64(value.tool_calls, "task estimated tool calls")?,
            retrieved_bytes: to_i64(value.retrieved_bytes, "task estimated retrieved bytes")?,
        })
    }
}

pub(super) fn reconcile_outcome_usage(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    outcome: &ExecutionTaskOutcome,
    terminal: bool,
) -> Option<BudgetReconciliation> {
    let mut ledger = budget_ledger(run);
    ledger
        .reconcile_cumulative_with_ceiling(
            task.reserved,
            &task.actual,
            &outcome.usage,
            terminal,
            i64::MAX as u64,
        )
        .ok()
}
