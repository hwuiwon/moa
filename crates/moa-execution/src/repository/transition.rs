//! Shared run/task transition evidence and batch cancellation mechanics.

use super::*;
use super::{
    materialize::reconcile_outcome_usage, outcome_support::outcome_projection_fields, rows::*,
    sql::*,
};

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

pub(super) fn task_has_accepted_audit_kind(task: &ExecutionTaskRecord, kind: &str) -> bool {
    task.outcome_audit.iter().any(|entry| {
        entry.get("kind").and_then(Value::as_str) == Some(kind)
            && entry.get("accepted").and_then(Value::as_bool) == Some(true)
    })
}

#[derive(Clone, Copy)]
pub(super) struct TaskCancellationEvidence<'a> {
    pub(super) kind: &'a str,
    pub(super) reason: &'a str,
    pub(super) base_plan_revision: Option<u64>,
    pub(super) amendment_hash: Option<&'a ExecutionHash>,
    pub(super) terminal_status: Option<ExecutionRunStatus>,
    pub(super) terminal_projection: Option<&'a TerminalProjection>,
    pub(super) completion_evaluation: Option<&'a CompletionEvaluation>,
}

pub(super) struct TaskCancellationWrite {
    pub(super) tasks: Vec<ExecutionTaskRecord>,
    pub(super) run_reserved: ExecutionEstimate,
    pub(super) run_consumed: ExecutionEstimate,
    pub(super) budget_overrun: bool,
}

#[derive(Serialize)]
pub(super) struct TaskTerminalizationRow {
    ordinal: i64,
    task_id: Uuid,
    generation: i64,
    outcome: Value,
    error: Option<Value>,
    citations: Value,
    audit: Value,
}

pub(super) async fn terminalize_nonterminal_tasks(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    tasks: &[ExecutionTaskRecord],
    evidence: TaskCancellationEvidence<'_>,
) -> Result<TaskCancellationWrite> {
    let mut ledger = run.clone();
    let mut terminalization_rows = Vec::with_capacity(tasks.len());
    for (ordinal, task) in tasks.iter().enumerate() {
        let outcome = cancelled_task_outcome(evidence.reason.to_string(), task.actual.clone());
        let reconciliation =
            reconcile_outcome_usage(&ledger, task, &outcome, true).ok_or_else(|| {
                Error::InvalidRepositoryData {
                    message: format!(
                        "{} cancellation could not reconcile task {}",
                        evidence.kind, task.task_id
                    ),
                }
            })?;
        let audit = json!({
            "kind": evidence.kind,
            "attempt": task.attempt,
            "generation": task.generation,
            "plan_revision": task.plan_revision,
            "base_plan_revision": evidence.base_plan_revision,
            "amendment_hash": evidence.amendment_hash,
            "accepted": true,
            "reason": evidence.reason,
            "terminal_status": evidence.terminal_status.map(ExecutionRunStatus::as_str),
            "terminal_projection": evidence.terminal_projection,
            "completion_evaluation": evidence.completion_evaluation,
            "outcome": &outcome,
            "recorded_at": Utc::now(),
        });
        let (_, error, citations) = outcome_projection_fields(&outcome)?;
        let ordinal = i64::try_from(ordinal).map_err(|_| Error::InvalidRepositoryData {
            message: "task terminalization ordinal exceeds PostgreSQL BIGINT".to_string(),
        })?;
        terminalization_rows.push(TaskTerminalizationRow {
            ordinal,
            task_id: task.task_id.as_uuid(),
            generation: to_i64(task.generation, "expected task generation")?,
            outcome: serde_json::to_value(&outcome)?,
            error,
            citations: serde_json::to_value(citations)?,
            audit,
        });
        ledger.reserved = reconciliation.run_reserved;
        ledger.consumed = reconciliation.run_consumed;
        ledger.budget_overrun = reconciliation.budget_overrun;
    }
    let terminalization_batch = serde_json::to_value(terminalization_rows)?;
    let rows = sqlx::query(TERMINALIZE_CANCELLED_TASK_BATCH_SQL)
        .bind(run.run_uid)
        .bind(terminalization_batch)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    if rows.len() != tasks.len() {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "{} cancellation terminalized {} of {} locked tasks",
                evidence.kind,
                rows.len(),
                tasks.len()
            ),
        });
    }
    let terminalized = rows
        .iter()
        .zip(tasks)
        .map(|(row, expected)| {
            let terminalized = task_from_row(row)?;
            if terminalized.task_id != expected.task_id
                || terminalized.generation != expected.generation
            {
                return Err(Error::InvalidRepositoryData {
                    message: format!(
                        "{} cancellation returned task {} generation {} instead of {} generation {}",
                        evidence.kind,
                        terminalized.task_id,
                        terminalized.generation,
                        expected.task_id,
                        expected.generation
                    ),
                });
            }
            Ok(terminalized)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(TaskCancellationWrite {
        tasks: terminalized,
        run_reserved: ledger.reserved,
        run_consumed: ledger.consumed,
        budget_overrun: ledger.budget_overrun,
    })
}
