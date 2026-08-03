//! Outcome audit, replay, and terminal-rejection persistence helpers.

use super::*;
use super::{materialize::reconcile_outcome_usage, rows::*, sql::*};

pub(super) fn usage_is_cumulative(previous: &ExecutionUsage, cumulative: &ExecutionUsage) -> bool {
    cumulative.cost_microusd >= previous.cost_microusd
        && cumulative.tokens >= previous.tokens
        && cumulative.tool_calls >= previous.tool_calls
        && cumulative.retrieved_bytes >= previous.retrieved_bytes
}

pub(super) fn task_failure_fingerprint_input(
    task: &ExecutionTaskRecord,
) -> Option<FailureFingerprintInput> {
    let outcome = task.current_outcome.as_ref()?;
    let (class, message) = match &outcome.result {
        ExecutionTaskResult::Failed { class, message } => (class.clone(), message.clone()),
        ExecutionTaskResult::NeedsReplan { reason, .. } => (
            moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
            reason.clone(),
        ),
        ExecutionTaskResult::Completed { .. }
        | ExecutionTaskResult::NeedsInput { .. }
        | ExecutionTaskResult::Cancelled { .. } => return None,
    };
    Some(FailureFingerprintInput {
        class,
        node_id: task.node_id.clone(),
        capability_ref: None,
        message,
    })
}

pub(super) fn resume_budget_exhausted(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
) -> bool {
    run.budget_overrun
        || resource_dimension_exhausted(
            run.approved_budget.max_cost_microusd,
            run.consumed.cost_microusd,
            run.reserved.cost_microusd,
            task.estimate.cost_microusd,
            task.reserved.cost_microusd,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_tokens,
            run.consumed.tokens,
            run.reserved.tokens,
            task.estimate.tokens,
            task.reserved.tokens,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_tool_calls,
            run.consumed.tool_calls,
            run.reserved.tool_calls,
            task.estimate.tool_calls,
            task.reserved.tool_calls,
        )
        || resource_dimension_exhausted(
            run.approved_budget.max_retrieved_bytes,
            run.consumed.retrieved_bytes,
            run.reserved.retrieved_bytes,
            task.estimate.retrieved_bytes,
            task.reserved.retrieved_bytes,
        )
}

pub(super) fn resource_dimension_exhausted(
    limit: Option<u64>,
    consumed: u64,
    reserved: u64,
    task_estimate: u64,
    task_reserved: u64,
) -> bool {
    task_estimate > 0
        && task_reserved == 0
        && limit.is_some_and(|limit| consumed.saturating_add(reserved) >= limit)
}

pub(super) fn outcome_projection_fields(
    outcome: &ExecutionTaskOutcome,
) -> Result<(Option<Value>, Option<Value>, Vec<ExecutionCitation>)> {
    match &outcome.result {
        ExecutionTaskResult::Completed { output, citations } => {
            Ok((Some(output.clone()), None, citations.clone()))
        }
        ExecutionTaskResult::NeedsInput { question, audience } => Ok((
            None,
            Some(json!({ "question": question, "audience": audience })),
            Vec::new(),
        )),
        ExecutionTaskResult::NeedsReplan { reason, evidence } => Ok((
            None,
            Some(json!({ "reason": reason, "evidence": evidence })),
            Vec::new(),
        )),
        ExecutionTaskResult::Cancelled { reason } => Ok((
            None,
            Some(json!({ "class": "cancelled", "message": reason })),
            Vec::new(),
        )),
        ExecutionTaskResult::Failed { class, message } => Ok((
            None,
            Some(json!({ "class": class, "message": message })),
            Vec::new(),
        )),
    }
}

pub(super) fn outcome_audit_entry(
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
    accepted: bool,
    rejection: Option<TaskOutcomeRejection>,
) -> Value {
    json!({
        "received_attempt": attempt_for_generation(task, generation),
        "received_generation": generation,
        "accepted": accepted,
        "rejection": rejection,
        "outcome": outcome,
        "recorded_at": Utc::now(),
    })
}

pub(super) fn attempt_for_generation(task: &ExecutionTaskRecord, generation: u64) -> Option<u64> {
    task.generation_history.iter().find_map(|entry| {
        (entry.get("generation").and_then(Value::as_u64) == Some(generation))
            .then(|| entry.get("attempt").and_then(Value::as_u64))
            .flatten()
    })
}

pub(super) async fn append_outcome_audit(
    conn: &mut ScopedConn<'_>,
    task: &ExecutionTaskRecord,
    generation: u64,
    outcome: &ExecutionTaskOutcome,
    accepted: bool,
    rejection: Option<TaskOutcomeRejection>,
) -> Result<ExecutionTaskRecord> {
    let audit = outcome_audit_entry(task, generation, outcome, accepted, rejection);
    let row = sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(audit)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    task_from_row(&row)
}

pub(super) fn terminal_reservation_rejection(
    task: &ExecutionTaskRecord,
) -> Option<ReservationRejection> {
    if task.status != ExecutionTaskStatus::Failed {
        return None;
    }
    let audit = task.outcome_audit.last()?;
    if audit.get("kind").and_then(Value::as_str) != Some("reservation_admission_rejected")
        || audit.get("generation").and_then(Value::as_u64) != Some(task.generation)
    {
        return None;
    }
    match audit.get("rejection").and_then(Value::as_str) {
        Some("DeadlineElapsed") => Some(ReservationRejection::DeadlineElapsed),
        Some("BudgetExceeded") => Some(ReservationRejection::BudgetExceeded),
        _ => None,
    }
}

pub(super) async fn terminalize_reservation_rejection(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    rejection: ReservationRejection,
) -> Result<ReservationTerminalization> {
    let class = match rejection {
        ReservationRejection::DeadlineElapsed => {
            moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded
        }
        ReservationRejection::BudgetExceeded => {
            moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded
        }
        _ => {
            return Err(Error::InvalidRepositoryInput {
                message: "only deadline or budget admission failures may terminalize a task"
                    .to_string(),
            });
        }
    };
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: task.actual.clone(),
        result: ExecutionTaskResult::Failed {
            class,
            message: format!("execution task reservation rejected: {rejection:?}"),
        },
    };
    // The database state machine requires failed tasks to pass through reserved and running.
    // These transitions stay inside this transaction and intentionally set no dispatch
    // timestamps or resource reservations because admission rejected the work before dispatch.
    let reserved = sqlx::query(
        "UPDATE moa.execution_task SET status = 'reserved', updated_at = NOW() \
         WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'pending'",
    )
    .bind(task.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.generation, "task generation")?)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if reserved.rows_affected() != 1 {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked pending task".to_string(),
        });
    }
    let running = sqlx::query(
        "UPDATE moa.execution_task SET status = 'running', updated_at = NOW() \
         WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'reserved'",
    )
    .bind(task.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.generation, "task generation")?)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if running.rows_affected() != 1 {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked reserved task".to_string(),
        });
    }
    sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
        .bind(run.run_uid)
        .bind(run.status.as_str())
        .bind(to_i64(run.reserved.cost_microusd, "run reserved cost")?)
        .bind(to_i64(run.reserved.tokens, "run reserved tokens")?)
        .bind(to_i64(run.reserved.tasks, "run reserved tasks")?)
        .bind(to_i64(run.reserved.tool_calls, "run reserved tool calls")?)
        .bind(to_i64(
            run.reserved.retrieved_bytes,
            "run reserved retrieved bytes",
        )?)
        .bind(to_i64(run.consumed.cost_microusd, "run consumed cost")?)
        .bind(to_i64(run.consumed.tokens, "run consumed tokens")?)
        .bind(to_i64(run.consumed.tasks, "run consumed tasks")?)
        .bind(to_i64(run.consumed.tool_calls, "run consumed tool calls")?)
        .bind(to_i64(
            run.consumed.retrieved_bytes,
            "run consumed retrieved bytes",
        )?)
        .bind(run.budget_overrun)
        .bind(0_i64)
        .bind(1_i64)
        .bind(0_i64)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;

    let (_, error, citations) = outcome_projection_fields(&outcome)?;
    let audit = json!({
        "kind": "reservation_admission_rejected",
        "attempt": task.attempt,
        "generation": task.generation,
        "accepted": true,
        "rejection": format!("{rejection:?}"),
        "outcome": &outcome,
        "recorded_at": Utc::now(),
    });
    let task_row = sqlx::query(RECORD_RESERVATION_REJECTION_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(to_i64(task.generation, "task generation")?)
        .bind(to_i64(task.actual.cost_microusd, "actual task cost")?)
        .bind(to_i64(task.actual.tokens, "actual task tokens")?)
        .bind(to_i64(task.actual.tool_calls, "actual task tool calls")?)
        .bind(to_i64(
            task.actual.retrieved_bytes,
            "actual task retrieved bytes",
        )?)
        .bind(serde_json::to_value(&outcome)?)
        .bind(error)
        .bind(serde_json::to_value(citations)?)
        .bind(audit)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let Some(task_row) = task_row else {
        return Err(Error::Storage {
            message: "reservation terminalization lost its locked generation fence".to_string(),
        });
    };
    let run_row = sqlx::query(LOAD_RUN_SQL)
        .bind(run.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    Ok(ReservationTerminalization {
        run: run_from_row(&run_row)?,
        task: task_from_row(&task_row)?,
        rejection,
    })
}

pub(super) fn redispatch_is_exact_replay(
    task: &ExecutionTaskRecord,
    history_kind: &str,
    requested_generation: u64,
    resume_input: Option<&Value>,
) -> bool {
    let Some(history) = task.generation_history.last() else {
        return false;
    };
    let matches_generation = history.get("kind").and_then(Value::as_str) == Some(history_kind)
        && history.get("requested_generation").and_then(Value::as_u64)
            == Some(requested_generation)
        && history.get("generation").and_then(Value::as_u64) == Some(task.generation);
    if !matches_generation {
        return false;
    }
    history_kind != "input_resume" || task.resume_input_history.last() == resume_input
}

pub(super) async fn terminalize_redispatch_rejection(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    history_kind: &str,
    class: moa_artifacts::execution_plan::ExecutionFailureClass,
    reason: TransitionRejection,
) -> Result<ExecutionTaskRecord> {
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: task.actual.clone(),
        result: ExecutionTaskResult::Failed {
            class,
            message: format!("execution task {history_kind} rejected: {reason:?}"),
        },
    };
    let reconciliation = reconcile_outcome_usage(run, task, &outcome, true).ok_or_else(|| {
        Error::InvalidRepositoryData {
            message: "redispatch rejection could not reconcile cumulative usage".to_string(),
        }
    })?;
    sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
        .bind(run.run_uid)
        .bind(ExecutionRunStatus::Running.as_str())
        .bind(to_i64(
            reconciliation.run_reserved.cost_microusd,
            "run reserved cost",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tokens,
            "run reserved tokens",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tasks,
            "run reserved tasks",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.tool_calls,
            "run reserved tool calls",
        )?)
        .bind(to_i64(
            reconciliation.run_reserved.retrieved_bytes,
            "run reserved retrieved bytes",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.cost_microusd,
            "run consumed cost",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tokens,
            "run consumed tokens",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tasks,
            "run consumed tasks",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.tool_calls,
            "run consumed tool calls",
        )?)
        .bind(to_i64(
            reconciliation.run_consumed.retrieved_bytes,
            "run consumed retrieved bytes",
        )?)
        .bind(reconciliation.budget_overrun)
        .bind(0_i64)
        .bind(1_i64)
        .bind(0_i64)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;

    let (_, error, citations) = outcome_projection_fields(&outcome)?;
    let audit = outcome_audit_entry(task, task.generation, &outcome, true, None);
    let row = sqlx::query(RECORD_TASK_OUTCOME_SQL)
        .bind(task.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(to_i64(task.generation, "task generation")?)
        .bind(ExecutionTaskStatus::Failed.as_str())
        .bind(to_i64(
            reconciliation.remaining_task_reservation.cost_microusd,
            "remaining task cost reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tokens,
            "remaining task token reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tasks,
            "remaining task logical reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.tool_calls,
            "remaining task tool-call reservation",
        )?)
        .bind(to_i64(
            reconciliation.remaining_task_reservation.retrieved_bytes,
            "remaining task byte reservation",
        )?)
        .bind(to_i64(task.actual.cost_microusd, "actual task cost")?)
        .bind(to_i64(task.actual.tokens, "actual task tokens")?)
        .bind(1_i64)
        .bind(to_i64(task.actual.tool_calls, "actual task tool calls")?)
        .bind(to_i64(
            task.actual.retrieved_bytes,
            "actual task retrieved bytes",
        )?)
        .bind(serde_json::to_value(&outcome)?)
        .bind(Option::<Value>::None)
        .bind(error)
        .bind(serde_json::to_value(citations)?)
        .bind(audit)
        .bind(true)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    task_from_row(&row)
}
