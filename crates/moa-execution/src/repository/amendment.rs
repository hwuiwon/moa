//! Bounded persisted evidence for restricted execution-plan amendments.

use std::collections::BTreeMap;

use moa_config::ExecutionConfig;

use super::*;
use super::{outcome_support::task_failure_fingerprint_input, projection::budget_ledger, rows::*};
use crate::{replan::failure_fingerprint, state::ExecutionAmendmentProjection};

/// One session- and revision-fenced amendment evidence request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AmendmentProjectionRequest {
    /// Run being amended.
    pub run_uid: Uuid,
    /// Parent session that owns the mutation boundary.
    pub session_id: SessionId,
    /// Exact active plan revision accepted by the amendment.
    pub expected_plan_revision: u64,
}

/// Compact compiler and loop-stop evidence loaded in one strictly bounded transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionAmendmentSnapshot {
    /// Canonical run, immutable pins, and exact generation counters.
    pub run: ExecutionRunRecord,
    /// Current persisted budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Compiler-bounded aggregate node state and exact replan origin.
    pub projection: ExecutionAmendmentProjection,
    /// Prior persisted occurrences of the current normalized failure fingerprint.
    pub prior_failure_fingerprint_counts: BTreeMap<ExecutionHash, u32>,
}

/// Result of the one-call bounded amendment projection load.
#[derive(Clone, Debug, PartialEq)]
pub enum AmendmentProjectionOutcome {
    /// Every bounded evidence source is ready for pure amendment validation.
    Ready(Box<ExecutionAmendmentSnapshot>),
    /// No run exists under the supplied tenant/contact/session scope.
    NotFound,
    /// The requested plan revision or replan origin is no longer current.
    Conflict,
}

impl ExecutionRepository {
    /// Loads compiler-bounded node aggregates, one replan task, and one indexed scalar count.
    pub async fn load_amendment_projection_for_session(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: AmendmentProjectionRequest,
    ) -> Result<AmendmentProjectionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(
            "SELECT * FROM moa.execution_run WHERE run_uid=$1 AND session_id=$2 FOR UPDATE",
        )
        .bind(request.run_uid)
        .bind(request.session_id.0)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentProjectionOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != request.expected_plan_revision
            || run.status != ExecutionRunStatus::WaitingReplan
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentProjectionOutcome::Conflict);
        }
        if run.active_plan.definition.nodes.len() > config.maximum_activation_steps {
            return Err(Error::InvalidRepositoryData {
                message: "persisted plan exceeds its compiler-validated node activation bound"
                    .to_string(),
            });
        }

        let replan_rows = sqlx::query(
            "SELECT *, failure_fingerprint AS persisted_failure_fingerprint \
             FROM moa.execution_task WHERE run_uid=$1 AND status='waiting_replan' \
             ORDER BY task_id LIMIT 2",
        )
        .bind(run.run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let [replan_row] = replan_rows.as_slice() else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentProjectionOutcome::Conflict);
        };
        let replan_task = task_from_row(replan_row)?;
        let failure = task_failure_fingerprint_input(&replan_task).ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "waiting-replan task has no fingerprintable persisted outcome".to_string(),
            }
        })?;
        let fingerprint = failure_fingerprint(&failure)?;
        let fingerprint_text = fingerprint.to_string();
        let persisted_fingerprint: Option<String> = replan_row
            .try_get("persisted_failure_fingerprint")
            .map_err(row_error)?;
        if persisted_fingerprint.as_deref() != Some(fingerprint_text.as_str()) {
            return Err(Error::InvalidRepositoryData {
                message: "waiting-replan task failure fingerprint does not match its outcome"
                    .to_string(),
            });
        }
        let prior_count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.execution_task \
             WHERE run_uid=$1 AND task_id<>$2 AND failure_fingerprint=$3",
        )
        .bind(run.run_uid)
        .bind(replan_task.task_id.as_uuid())
        .bind(&fingerprint_text)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let prior_count = u32::try_from(prior_count).map_err(|_| Error::ArithmeticOverflow {
            context: "prior amendment failure fingerprint count".to_string(),
        })?;

        let node_limit = i64::try_from(config.maximum_activation_steps)
            .map_err(|_| Error::ArithmeticOverflow {
                context: "amendment node activation bound".to_string(),
            })?
            .checked_add(1)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "amendment node activation bound".to_string(),
            })?;
        let node_rows = sqlx::query(
            "SELECT node_id,node_status,total_task_count FROM moa.execution_node_state \
             WHERE run_uid=$1 AND node_id NOT LIKE '@check/%' \
             ORDER BY node_order,node_state_uid LIMIT $2",
        )
        .bind(run.run_uid)
        .bind(node_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if node_rows.len() != run.active_plan.definition.nodes.len()
            || node_rows.len() > config.maximum_activation_steps
        {
            return Err(Error::InvalidRepositoryData {
                message: "amendment node aggregates do not exactly cover the bounded active plan"
                    .to_string(),
            });
        }
        let mut node_statuses = BTreeMap::new();
        let mut started_node_ids = std::collections::BTreeSet::new();
        for row in node_rows {
            let node_id: String = row.try_get("node_id").map_err(row_error)?;
            let status = amendment_node_status(
                &row.try_get::<String, _>("node_status").map_err(row_error)?,
            )?;
            if status != ExecutionNodeStatus::Pending || required_u64(&row, "total_task_count")? > 0
            {
                started_node_ids.insert(node_id.clone());
            }
            node_statuses.insert(node_id, status);
        }
        let projection = ExecutionAmendmentProjection {
            plan_revision: run.plan_revision,
            node_statuses,
            started_node_ids,
            replan_tasks: vec![task_projection(&replan_task)],
        };
        let snapshot = ExecutionAmendmentSnapshot {
            budget_ledger: budget_ledger(&run),
            run,
            projection,
            prior_failure_fingerprint_counts: BTreeMap::from([(fingerprint, prior_count)]),
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(AmendmentProjectionOutcome::Ready(Box::new(snapshot)))
    }
}

fn task_projection(task: &ExecutionTaskRecord) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: task.task_id,
        node_id: task.node_id.clone(),
        item_key: task.item_key.clone(),
        status: task.status,
        attempt: task.attempt,
        generation: task.generation,
        input: task.input.clone(),
        outcome: task.current_outcome.clone(),
    }
}

fn amendment_node_status(value: &str) -> Result<ExecutionNodeStatus> {
    match value {
        "pending" => Ok(ExecutionNodeStatus::Pending),
        "ready" | "running" => Ok(ExecutionNodeStatus::Running),
        "waiting" => Ok(ExecutionNodeStatus::Waiting),
        "completed" => Ok(ExecutionNodeStatus::Completed),
        "skipped" => Ok(ExecutionNodeStatus::Skipped),
        "failed" => Ok(ExecutionNodeStatus::Failed),
        "cancelled" => Ok(ExecutionNodeStatus::Cancelled),
        other => Err(Error::InvalidRepositoryData {
            message: format!("unknown amendment node status `{other}`"),
        }),
    }
}
