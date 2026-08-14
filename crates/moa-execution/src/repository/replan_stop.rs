//! Durable bounded replan-stop intent handoff.

use moa_core::types::identifiers::SessionId;

use super::*;
use super::{
    capacity::{ExecutionCapacityDimension, prelock_capacity_dimensions_in_tx},
    ready::cancel_unmaterialized_dependents_in_tx,
    rows::{required_u64, run_from_row},
    run::enqueue_run_activation_in_conn,
    sql::LOAD_RUN_SQL,
    terminal::ReplanStopReceipt,
};
use crate::ReplanStopReason;

const MAX_REPLAN_STOP_DETAIL_CHARS: usize = 512;

/// Exact task- and revision-fenced intent requested by the public amendment boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewExecutionReplanStopIntent {
    /// Owning run.
    pub run_uid: Uuid,
    /// Parent session that owns the request.
    pub session_id: SessionId,
    /// Expected active plan revision.
    pub base_plan_revision: u64,
    /// WaitingReplan task that triggered the stop.
    pub origin_task_id: ExecutionTaskId,
    /// Exact logical task generation.
    pub task_generation: u64,
    /// Deterministic amendment hash associated with the stop.
    pub amendment_hash: ExecutionHash,
    /// Typed deterministic stop reason.
    pub stop_reason: ReplanStopReason,
    /// Optional bounded diagnostic detail.
    pub detail: Option<String>,
}

/// Persisted intent consumed only by its exact claimed controller wake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionReplanStopIntentRecord {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning run.
    pub run_uid: Uuid,
    /// Exact controller generation.
    pub controller_generation: u64,
    /// Exact controller wake.
    pub wake_epoch: u64,
    /// WaitingReplan origin.
    pub origin_task_id: ExecutionTaskId,
    /// Exact logical task generation.
    pub task_generation: u64,
    /// Plan revision that stopped.
    pub base_plan_revision: u64,
    /// Typed stop reason.
    pub stop_reason: ReplanStopReason,
    /// Bounded human-readable detail.
    pub detail: String,
    /// Exact amendment hash.
    pub amendment_hash: ExecutionHash,
}

impl ExecutionReplanStopIntentRecord {
    /// Reconstructs the exact receipt eventually written with the terminal fence.
    #[must_use]
    pub const fn receipt(&self) -> ReplanStopReceipt {
        ReplanStopReceipt {
            task_id: self.origin_task_id,
            task_generation: self.task_generation,
            base_plan_revision: self.base_plan_revision,
            amendment_hash: self.amendment_hash,
        }
    }
}

/// Result of persisting one exact replan-stop intent and activation.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplanStopIntentWriteOutcome {
    /// New intent and exact activation committed atomically.
    Applied(Box<ExecutionRunRecord>),
    /// The identical intent was already persisted.
    Replayed(Box<ExecutionRunRecord>),
    /// Run/session does not exist under the supplied scope.
    NotFound,
    /// Revision, task, generation, or an existing intent differs.
    Conflict,
}

impl ExecutionRepository {
    /// Persists one exact replan-stop command and queues its owning controller wake atomically.
    pub async fn request_replan_stop(
        &self,
        scope: ExecutionScope,
        config: &moa_config::ExecutionConfig,
        request: NewExecutionReplanStopIntent,
    ) -> Result<ReplanStopIntentWriteOutcome> {
        let detail = request
            .detail
            .as_deref()
            .filter(|detail| !detail.trim().is_empty())
            .unwrap_or(request.stop_reason.as_str())
            .chars()
            .take(MAX_REPLAN_STOP_DETAIL_CHARS)
            .collect::<String>();
        let mut conn = scope.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_run WHERE run_uid=$1 AND session_id=$2",
        )
        .bind(request.run_uid)
        .bind(request.session_id.0)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopIntentWriteOutcome::NotFound);
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
            return Ok(ReplanStopIntentWriteOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let existing = sqlx::query(
            "SELECT tenant_id,run_uid,controller_generation,wake_epoch,origin_task_id, \
                    task_generation,base_plan_revision,stop_reason,detail,amendment_hash \
             FROM moa.execution_replan_stop_intent WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if let Some(existing) = existing {
            let existing = replan_stop_intent_from_row(&existing)?;
            let exact = existing.origin_task_id == request.origin_task_id
                && existing.task_generation == request.task_generation
                && existing.base_plan_revision == request.base_plan_revision
                && existing.amendment_hash == request.amendment_hash
                && existing.stop_reason == request.stop_reason
                && existing.detail == detail;
            conn.commit().await.map_err(storage_error)?;
            return Ok(if exact {
                ReplanStopIntentWriteOutcome::Replayed(Box::new(run))
            } else {
                ReplanStopIntentWriteOutcome::Conflict
            });
        }
        if run.plan_revision != request.base_plan_revision
            || run.status != ExecutionRunStatus::WaitingReplan
            || run.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopIntentWriteOutcome::Conflict);
        }
        let task = sqlx::query(
            "SELECT node_id,generation,status,current_outcome FROM moa.execution_task \
             WHERE run_uid=$1 AND task_id=$2 FOR UPDATE",
        )
        .bind(run.run_uid)
        .bind(request.origin_task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(task) = task else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopIntentWriteOutcome::Conflict);
        };
        let task_generation = required_u64(&task, "generation")?;
        let origin_node_id: String = task.try_get("node_id").map_err(row_error)?;
        let task_status: String = task.try_get("status").map_err(row_error)?;
        let current_outcome: Option<ExecutionTaskOutcome> = task
            .try_get::<Option<Value>, _>("current_outcome")
            .map_err(row_error)?
            .map(serde_json::from_value)
            .transpose()?;
        if task_generation != request.task_generation
            || task_status != "waiting_replan"
            || !matches!(
                current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::NeedsReplan { .. })
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopIntentWriteOutcome::Conflict);
        }
        cancel_unmaterialized_dependents_in_tx(conn.as_mut(), &run, &origin_node_id).await?;

        let dispatch = enqueue_run_activation_in_conn(
            conn.as_mut(),
            run.tenant_id,
            run.run_uid,
            run.controller_generation,
            Utc::now(),
            json!({
                "reason": "replan_stop_completion",
                "base_plan_revision": request.base_plan_revision,
                "origin_task_id": request.origin_task_id,
            }),
        )
        .await?;
        let wake_epoch = dispatch
            .wake_epoch
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "replan-stop activation is missing its wake epoch".to_string(),
            })?;
        sqlx::query(
            "INSERT INTO moa.execution_replan_stop_intent (tenant_id,run_uid, \
                 controller_generation,wake_epoch,origin_task_id,task_generation, \
                 base_plan_revision,stop_reason,detail,amendment_hash) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(to_i64(
            run.controller_generation,
            "replan-stop controller generation",
        )?)
        .bind(to_i64(wake_epoch, "replan-stop wake epoch")?)
        .bind(request.origin_task_id.as_uuid())
        .bind(to_i64(
            request.task_generation,
            "replan-stop task generation",
        )?)
        .bind(to_i64(
            request.base_plan_revision,
            "replan-stop plan revision",
        )?)
        .bind(request.stop_reason.as_str())
        .bind(detail)
        .bind(request.amendment_hash.to_string())
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = sqlx::query(LOAD_RUN_SQL)
            .bind(run.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)
            .and_then(|row| run_from_row(&row))?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ReplanStopIntentWriteOutcome::Applied(Box::new(run)))
    }

    /// Loads an intent only for the exact claimed controller generation and wake.
    pub async fn load_replan_stop_intent(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        wake_epoch: u64,
    ) -> Result<Option<ExecutionReplanStopIntentRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(
            "SELECT tenant_id,run_uid,controller_generation,wake_epoch,origin_task_id, \
                    task_generation,base_plan_revision,stop_reason,detail,amendment_hash \
             FROM moa.execution_replan_stop_intent WHERE run_uid=$1 \
               AND controller_generation=$2 AND wake_epoch=$3",
        )
        .bind(run_uid)
        .bind(to_i64(
            controller_generation,
            "replan-stop controller generation",
        )?)
        .bind(to_i64(wake_epoch, "replan-stop wake epoch")?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let intent = row.as_ref().map(replan_stop_intent_from_row).transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(intent)
    }
}

fn replan_stop_intent_from_row(row: &PgRow) -> Result<ExecutionReplanStopIntentRecord> {
    Ok(ExecutionReplanStopIntentRecord {
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        controller_generation: required_u64(row, "controller_generation")?,
        wake_epoch: required_u64(row, "wake_epoch")?,
        origin_task_id: ExecutionTaskId::from_uuid(
            row.try_get("origin_task_id").map_err(row_error)?,
        ),
        task_generation: required_u64(row, "task_generation")?,
        base_plan_revision: required_u64(row, "base_plan_revision")?,
        stop_reason: parse_replan_stop_reason(
            &row.try_get::<String, _>("stop_reason").map_err(row_error)?,
        )?,
        detail: row.try_get("detail").map_err(row_error)?,
        amendment_hash: row
            .try_get::<String, _>("amendment_hash")
            .map_err(row_error)?
            .parse()?,
    })
}

fn parse_replan_stop_reason(value: &str) -> Result<ReplanStopReason> {
    match value {
        "duplicate_plan" => Ok(ReplanStopReason::DuplicatePlan),
        "duplicate_amendment" => Ok(ReplanStopReason::DuplicateAmendment),
        "repeated_failure" => Ok(ReplanStopReason::RepeatedFailure),
        "no_progress" => Ok(ReplanStopReason::NoProgress),
        "deadline_exceeded" => Ok(ReplanStopReason::DeadlineExceeded),
        "budget_exhausted" => Ok(ReplanStopReason::BudgetExhausted),
        other => Err(Error::InvalidRepositoryData {
            message: format!("unknown replan-stop reason `{other}`"),
        }),
    }
}
