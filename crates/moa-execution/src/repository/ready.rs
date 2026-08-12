//! Bounded ready-queue materialization and persisted per-node scheduler aggregates.

use std::collections::{BTreeMap, BTreeSet};

use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionTemporalTarget, ExecutionUsage, ExecutionWaitPolicy,
    InputAudience,
};
use moa_config::ExecutionConfig;
use sqlx::{Row, postgres::PgRow};

use crate::capability::node_output_hash;
use crate::interpreter::TemporalTargetResolution;
use crate::schema::validate_instance;

use super::*;
use super::{
    materialize::{ensure_materialization_replay_matches, prepare_task_materialization_batch},
    outcome_support::outcome_projection_fields,
    rows::*,
    sql::*,
    trigger::{ExecutionTriggerKind, NewExecutionTrigger, create_trigger_with_dispatch_in_conn},
};

const MAX_READY_PAGE_SIZE: u32 = 1_000;
const MAX_READY_PAGE_SIZE_USIZE: usize = 1_000;
const MAX_MAP_AGGREGATE_PAGE_SIZE: i64 = 16;
const MAX_ACTIVATION_OUTPUT_BYTES: i64 = 1_048_576;
const MAX_WAITING_REASON_SAMPLES: usize = 64;
const MAX_WAITING_REASON_SAMPLE_BYTES: usize = 65_536;

/// Durable scheduler lifecycle for one plan node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionNodeQueueStatus {
    /// Dependencies or conditions are not ready.
    Pending,
    /// Bounded logical work is available for fleet admission.
    Ready,
    /// At least one attempt owns active capacity.
    Running,
    /// Work is parked in storage awaiting an external event.
    Waiting,
    /// The deterministic node aggregate completed.
    Completed,
    /// The node condition evaluated false.
    Skipped,
    /// Node work failed terminally.
    Failed,
    /// Node work was cancelled.
    Cancelled,
}

/// Persisted bounded scheduler aggregate for one plan node.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionNodeStateRecord {
    /// Owning run.
    pub run_uid: Uuid,
    /// Stable plan node identifier.
    pub node_id: String,
    /// Deterministic plan order.
    pub node_order: u64,
    /// Current aggregate lifecycle.
    pub status: ExecutionNodeQueueStatus,
    /// Next source item not yet materialized.
    pub materialization_cursor: u64,
    /// Whether the deterministic source has no further logical tasks to materialize.
    pub materialization_complete: bool,
    /// One-based reduce round, including the initial plan-input round.
    pub reduce_round: u64,
    /// Number of batches already materialized in the current reduce round.
    pub reduce_batch_cursor: u64,
    /// Input item count for the current reduce round, once known.
    pub reduce_round_input_count: Option<u64>,
    /// Tasks materialized in the current reduce round.
    pub reduce_round_task_count: u64,
    /// Terminal tasks in the current reduce round.
    pub reduce_round_terminal_task_count: u64,
    /// Whether every source batch in the current reduce round has been materialized.
    pub reduce_ready: bool,
    /// Dependencies still blocking the node.
    pub remaining_dependency_count: u64,
    /// Total materialized logical tasks.
    pub total_task_count: u64,
    /// Tasks available to fleet admission.
    pub ready_task_count: u64,
    /// Attempts consuming active capacity.
    pub active_task_count: u64,
    /// Tasks parked without active compute.
    pub waiting_task_count: u64,
    /// Terminal logical tasks.
    pub terminal_task_count: u64,
    /// Deterministic aggregate output persisted when the node completes.
    pub aggregate_output: Option<Value>,
    /// Canonical hash verified whenever an aggregate output is loaded.
    pub aggregate_output_hash: Option<ExecutionHash>,
    /// Last map item key durably appended to the bounded aggregate.
    pub aggregate_cursor_item_key: Option<String>,
    /// Whether the aggregate is final and safe for dependent-node resolution.
    pub aggregate_complete: bool,
}

/// One lightweight pending map-aggregation page owned by a claimed controller wake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapAggregatePageRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact active plan revision.
    pub plan_revision: u64,
    /// Exact controller generation.
    pub controller_generation: u64,
    /// Exact claimed wake.
    pub wake_epoch: u64,
    /// Map node being aggregated.
    pub node_id: String,
    /// Exact persisted item-key cursor observed by the controller.
    pub expected_cursor_item_key: Option<String>,
}

/// Lightweight map-aggregation work discovered without loading partial aggregate JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapAggregateCandidate {
    /// Map node identifier.
    pub node_id: String,
    /// Deterministic plan order.
    pub node_order: u64,
    /// Exact persisted aggregation cursor.
    pub cursor_item_key: Option<String>,
}

/// Result of one bounded map-aggregation page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapAggregatePageOutcome {
    /// One page committed, possibly including the final completion CAS.
    Applied {
        /// Cursor after this page.
        next_cursor_item_key: Option<String>,
        /// Number of task outputs appended by this page.
        aggregated_tasks: u32,
        /// Whether this page completed the node and released its dependents.
        aggregate_complete: bool,
    },
    /// The exact page was already committed before its response was observed.
    Replayed {
        /// Current persisted cursor.
        next_cursor_item_key: Option<String>,
        /// Whether the aggregate is already complete.
        aggregate_complete: bool,
    },
    /// The cumulative inline output exceeded the one-MiB aggregate ceiling.
    Overflow,
    /// Run, generation, wake, plan, node, or cursor is no longer current.
    Conflict,
    /// No run exists under the supplied tenant/contact scope.
    NotFound,
}

/// Exact persisted reduce-round fence supplied with one ready materialization page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionReduceMaterializationCursor {
    /// One-based round being materialized.
    pub round: u64,
    /// Number of batches already committed in this round.
    pub batch_cursor: u64,
    /// Total input values consumed by this round.
    pub round_input_count: u64,
}

/// Bounded prior-round output slice requested for one reduce materialization page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReduceRoundInputPageRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Reduce plan node identifier.
    pub node_id: String,
    /// Immediately preceding round whose outputs feed this page.
    pub source_round: u64,
    /// Persisted current-round cursor and input count.
    pub cursor: ExecutionReduceMaterializationCursor,
    /// Fixed reducer batch size.
    pub batch_size: u32,
    /// Maximum target batches materialized by this page.
    pub target_batch_limit: u32,
}

/// Atomic input for one bounded ready-page materialization transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadyMaterializationRequest {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Immutable plan revision fenced by the page.
    pub plan_revision: u64,
    /// Single plan node materialized by this page.
    pub node_id: String,
    /// Total committed task count before this page.
    pub expected_cursor: u64,
    /// Exact reduce-round source position, only for reduce nodes.
    pub reduce_cursor: Option<ExecutionReduceMaterializationCursor>,
    /// Whether this page reached the end of its deterministic node source.
    pub source_exhausted: bool,
    /// Aggregate output when the source completes without creating a logical task.
    pub terminal_output: Option<Value>,
    /// Whether the node's declared condition evaluated false and no work may exist.
    pub condition_skipped: bool,
    /// Bounded deterministic logical tasks in source order.
    pub tasks: Vec<LogicalTask>,
}

/// One bounded controller input page reconstructed without loading every task row.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionActivationProjection {
    /// Canonical run snapshot containing the plan and generation fences.
    pub run: ExecutionRunRecord,
    /// Bounded node aggregate page in deterministic plan order.
    pub nodes: Vec<ExecutionNodeStateRecord>,
    /// Exact completed outputs referenced by nodes in this page.
    pub referenced_outputs: BTreeMap<String, Value>,
    /// Whether another actionable node exists beyond this bounded page.
    pub has_more_actionable: bool,
}

/// Constant-size scheduler readiness summary used after each bounded activation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionActivationReadiness {
    /// At least one dependency-ready node still has source work to materialize.
    pub has_actionable_nodes: bool,
    /// At least one plan node has not reached a terminal aggregate status.
    pub has_unfinished_nodes: bool,
    /// At least one logical task remains outside a terminal task status.
    pub has_nonterminal_tasks: bool,
}

impl ExecutionActivationReadiness {
    /// Returns whether every node and task has reached an ordinary terminal boundary.
    #[must_use]
    pub const fn terminal_ready(self) -> bool {
        !self.has_unfinished_nodes && !self.has_nonterminal_tasks
    }
}

/// Result of atomically materializing one bounded page directly into the ready queue.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadyMaterializationOutcome {
    /// New logical tasks were inserted and made ready.
    Applied {
        /// Exact persisted task records in deterministic request order.
        tasks: Vec<ExecutionTaskRecord>,
        /// Cursor to supply when materializing the next page for this node.
        next_cursor: u64,
        /// Exact delayed trigger deliveries committed for storage-only waits.
        triggers: Vec<ExecutionScheduledTrigger>,
    },
    /// The exact page had already committed before the caller retried.
    Replayed {
        /// Exact persisted task records in deterministic request order.
        tasks: Vec<ExecutionTaskRecord>,
        /// Current cursor after the replayed page.
        next_cursor: u64,
        /// Exact delayed trigger deliveries reconstructed for replay.
        triggers: Vec<ExecutionScheduledTrigger>,
    },
    /// The node cursor, plan revision, or immutable task semantics changed.
    Conflict,
}

/// Exact delayed delivery a controller must schedule after the transaction commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionScheduledTrigger {
    /// Durable trigger-delivery dispatch identity.
    pub dispatch_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Immutable trigger identity.
    pub trigger_uid: Uuid,
    /// Exact absolute delivery time.
    pub due_at: DateTime<Utc>,
}

/// Bounded page used to verify that no nonterminal task remains before finalization.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTerminalVerificationPage {
    /// Nonterminal tasks found in this page.
    pub nonterminal_tasks: Vec<ExecutionTaskRecord>,
    /// Stable cursor for the next page, or `None` when verification is complete.
    pub next_cursor: Option<ExecutionTaskId>,
}

impl ExecutionRepository {
    /// Loads one bounded controller projection from node aggregates and exact dependencies.
    pub async fn load_activation_projection(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        limit: u32,
    ) -> Result<Option<ExecutionActivationProjection>> {
        let limit = limit.clamp(1, MAX_READY_PAGE_SIZE);
        let fetch_limit = i64::from(limit) + 1;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let run = run_from_row(&run_row)?;
        let rows = sqlx::query(
            "SELECT run_uid, node_id, node_order, node_status, materialization_cursor, \
                    materialization_complete, \
                    remaining_dependency_count, total_task_count, ready_task_count, \
                    active_task_count, waiting_task_count, terminal_task_count, \
                    aggregate_output, aggregate_output_hash, aggregate_cursor_item_key, \
                    aggregate_complete \
                    , reduce_round, reduce_batch_cursor, reduce_round_input_count, \
                    reduce_round_task_count, reduce_round_terminal_task_count, reduce_ready \
             FROM moa.execution_node_state WHERE run_uid = $1 \
               AND remaining_dependency_count = 0 AND NOT materialization_complete \
               AND node_status NOT IN ('completed', 'skipped', 'failed', 'cancelled') \
             ORDER BY updated_at, node_order, node_state_uid LIMIT $2",
        )
        .bind(run_uid)
        .bind(fetch_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let has_more = rows.len()
            > usize::try_from(limit).map_err(|_| Error::InvalidRepositoryInput {
                message: "activation node page does not fit in memory".to_string(),
            })?;
        let nodes = rows
            .iter()
            .take(
                usize::try_from(limit).map_err(|_| Error::InvalidRepositoryInput {
                    message: "activation node page does not fit in memory".to_string(),
                })?,
            )
            .map(node_state_from_row)
            .collect::<Result<Vec<_>>>()?;
        let page_node_ids = nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<BTreeSet<_>>();
        let referenced_node_ids = run
            .active_plan
            .definition
            .nodes
            .iter()
            .filter(|node| page_node_ids.contains(node.id.as_str()))
            .flat_map(|node| node.depends_on.iter().cloned())
            .collect::<BTreeSet<_>>();
        let referenced_outputs = if referenced_node_ids.is_empty() {
            BTreeMap::new()
        } else {
            let ids = referenced_node_ids.into_iter().collect::<Vec<_>>();
            let referenced_bytes = sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(pg_column_size(aggregate_output)), 0)::BIGINT \
                 FROM moa.execution_node_state WHERE run_uid = $1 \
                   AND node_id = ANY($2::TEXT[]) AND aggregate_output IS NOT NULL",
            )
            .bind(run_uid)
            .bind(&ids)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if referenced_bytes > MAX_ACTIVATION_OUTPUT_BYTES {
                return Err(Error::InvalidRepositoryData {
                    message: format!(
                        "activation dependency outputs exceed {MAX_ACTIVATION_OUTPUT_BYTES} bytes"
                    ),
                });
            }
            sqlx::query(
                "SELECT node_id, aggregate_output, aggregate_output_hash \
                 FROM moa.execution_node_state \
                 WHERE run_uid = $1 AND node_id = ANY($2::TEXT[]) \
                   AND node_status IN ('completed', 'skipped') ORDER BY node_order",
            )
            .bind(run_uid)
            .bind(&ids)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .into_iter()
            .map(|row| {
                let node_id: String = row.try_get("node_id").map_err(row_error)?;
                let output: Option<Value> = row.try_get("aggregate_output").map_err(row_error)?;
                let hash: Option<String> =
                    row.try_get("aggregate_output_hash").map_err(row_error)?;
                let output = output.unwrap_or(Value::Null);
                let hash = hash.ok_or_else(|| Error::InvalidRepositoryData {
                    message: format!("node `{node_id}` aggregate output is missing its hash"),
                })?;
                if node_output_hash(&output)?.to_string() != hash {
                    return Err(Error::InvalidRepositoryData {
                        message: format!("node `{node_id}` aggregate output hash mismatch"),
                    });
                }
                Ok((node_id, output))
            })
            .collect::<Result<_>>()?
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(Some(ExecutionActivationProjection {
            run,
            nodes,
            referenced_outputs,
            has_more_actionable: has_more,
        }))
    }

    /// Loads the oldest pending map aggregate without loading its partial JSON value.
    pub async fn load_map_aggregate_candidate(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        wake_epoch: u64,
    ) -> Result<Option<MapAggregateCandidate>> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let run = run_from_row(&run_row)?;
        if run.controller_generation != controller_generation
            || run.wake_epoch != wake_epoch
            || run.activation_state != ExecutionActivationState::Advancing
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT node_id,node_order,aggregate_cursor_item_key \
             FROM moa.execution_node_state WHERE run_uid=$1 AND materialization_complete \
               AND NOT aggregate_complete AND node_status='pending' \
               AND terminal_task_count=total_task_count AND total_task_count>0 \
             ORDER BY updated_at,node_order,node_state_uid LIMIT 1",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let candidate = row
            .map(|row| {
                let node_id: String = row.try_get("node_id").map_err(row_error)?;
                let is_map = run
                    .active_plan
                    .definition
                    .nodes
                    .iter()
                    .find(|node| node.id == node_id)
                    .is_some_and(|node| matches!(node.operation, ExecutionOperation::Map { .. }));
                if !is_map {
                    return Err(Error::InvalidRepositoryData {
                        message: format!(
                            "non-map node `{node_id}` entered the pending aggregate queue"
                        ),
                    });
                }
                Ok(MapAggregateCandidate {
                    node_id,
                    node_order: required_u64(&row, "node_order")?,
                    cursor_item_key: row
                        .try_get("aggregate_cursor_item_key")
                        .map_err(row_error)?,
                })
            })
            .transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(candidate)
    }

    /// Appends at most sixteen inline map outputs and completes the node only at source exhaustion.
    pub async fn advance_map_aggregate_page(
        &self,
        scope: ExecutionScope,
        request: MapAggregatePageRequest,
    ) -> Result<MapAggregatePageOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != request.plan_revision
            || run.controller_generation != request.controller_generation
            || run.wake_epoch != request.wake_epoch
            || run.activation_state != ExecutionActivationState::Advancing
            || run.status.is_terminal()
            || run.pending_terminal.is_some()
            || !run
                .active_plan
                .definition
                .nodes
                .iter()
                .find(|node| node.id == request.node_id)
                .is_some_and(|node| matches!(node.operation, ExecutionOperation::Map { .. }))
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Conflict);
        }
        let Some(node) = sqlx::query(
            "SELECT node_status,materialization_complete,total_task_count,terminal_task_count, \
                    succeeded_task_count, \
                    aggregate_output,aggregate_output_hash,aggregate_cursor_item_key,aggregate_complete \
             FROM moa.execution_node_state WHERE run_uid=$1 AND node_id=$2 FOR UPDATE",
        )
        .bind(request.run_uid)
        .bind(&request.node_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Conflict);
        };
        let current_cursor: Option<String> = node
            .try_get("aggregate_cursor_item_key")
            .map_err(row_error)?;
        let aggregate_complete: bool = node.try_get("aggregate_complete").map_err(row_error)?;
        if current_cursor != request.expected_cursor_item_key {
            let stale_replay = match (&current_cursor, &request.expected_cursor_item_key) {
                (Some(current), Some(expected)) => current > expected,
                (Some(_), None) => true,
                (None, Some(_)) | (None, None) => false,
            };
            conn.commit().await.map_err(storage_error)?;
            return Ok(if stale_replay {
                MapAggregatePageOutcome::Replayed {
                    next_cursor_item_key: current_cursor,
                    aggregate_complete,
                }
            } else {
                MapAggregatePageOutcome::Conflict
            });
        }
        if aggregate_complete {
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Replayed {
                next_cursor_item_key: current_cursor,
                aggregate_complete: true,
            });
        }
        let node_status: String = node.try_get("node_status").map_err(row_error)?;
        let materialization_complete: bool = node
            .try_get("materialization_complete")
            .map_err(row_error)?;
        let total_task_count = required_u64(&node, "total_task_count")?;
        if node_status != "pending"
            || !materialization_complete
            || total_task_count == 0
            || required_u64(&node, "terminal_task_count")? != total_task_count
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Conflict);
        }

        let rows = sqlx::query(
            "SELECT item_key,COALESCE(output,'null'::JSONB) AS output \
             FROM moa.execution_task WHERE run_uid=$1 AND node_id=$2 \
               AND status IN ('completed','skipped') \
               AND ($3::TEXT IS NULL OR item_key>$3) \
             ORDER BY item_key,task_id LIMIT $4",
        )
        .bind(request.run_uid)
        .bind(&request.node_id)
        .bind(&current_cursor)
        .bind(MAX_MAP_AGGREGATE_PAGE_SIZE)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let next_cursor = rows
            .last()
            .map(|row| row.try_get::<String, _>("item_key").map_err(row_error))
            .transpose()?
            .or_else(|| current_cursor.clone());
        let has_more = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 AND node_id=$2 \
               AND status IN ('completed','skipped') \
               AND ($3::TEXT IS NULL OR item_key>$3))",
        )
        .bind(request.run_uid)
        .bind(&request.node_id)
        .bind(&next_cursor)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let mut aggregate = match node
            .try_get::<Option<Value>, _>("aggregate_output")
            .map_err(row_error)?
        {
            Some(Value::Array(values)) => values,
            None => Vec::new(),
            Some(_) => {
                return Err(Error::InvalidRepositoryData {
                    message: "partial map aggregate is not a JSON array".to_string(),
                });
            }
        };
        let persisted_hash: Option<String> =
            node.try_get("aggregate_output_hash").map_err(row_error)?;
        if !aggregate.is_empty() {
            let current = Value::Array(aggregate.clone());
            let current_hash = node_output_hash(&current)?.to_string();
            if persisted_hash.as_deref() != Some(current_hash.as_str()) {
                return Err(Error::InvalidRepositoryData {
                    message: "partial map aggregate hash mismatch".to_string(),
                });
            }
        } else if persisted_hash.is_some() {
            return Err(Error::InvalidRepositoryData {
                message: "empty map aggregate unexpectedly has a persisted hash".to_string(),
            });
        }
        for row in &rows {
            aggregate.push(row.try_get("output").map_err(row_error)?);
        }
        let aggregate = Value::Array(aggregate);
        let aggregate_bytes = moa_core::canonical_json::canonical_json_bytes(&aggregate)?;
        if aggregate_bytes.len()
            > usize::try_from(MAX_ACTIVATION_OUTPUT_BYTES).map_err(|_| {
                Error::InvalidRepositoryData {
                    message: "activation output byte ceiling is invalid".to_string(),
                }
            })?
        {
            let updated = sqlx::query(
                "UPDATE moa.execution_node_state SET node_status='failed',aggregate_output=NULL, \
                     aggregate_output_hash=NULL,aggregate_complete=TRUE,updated_at=NOW() \
                 WHERE run_uid=$1 AND node_id=$2 \
                   AND aggregate_cursor_item_key IS NOT DISTINCT FROM $3 AND NOT aggregate_complete",
            )
            .bind(request.run_uid)
            .bind(&request.node_id)
            .bind(&current_cursor)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if updated.rows_affected() != 1 {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(MapAggregatePageOutcome::Conflict);
            }
            conn.commit().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Overflow);
        }
        let complete = !has_more;
        let completed_status = if required_u64(&node, "succeeded_task_count")? == 0 {
            "skipped"
        } else {
            "completed"
        };
        let persisted_output = if complete && completed_status == "skipped" {
            Value::Null
        } else {
            aggregate
        };
        let output_hash = node_output_hash(&persisted_output)?.to_string();
        let updated = sqlx::query(
            "UPDATE moa.execution_node_state SET aggregate_cursor_item_key=$4,aggregate_output=$5, \
                 aggregate_output_hash=$6,aggregate_complete=$7, \
                 node_status=CASE WHEN $7 THEN $8 ELSE 'pending' END,updated_at=NOW() \
             WHERE run_uid=$1 AND node_id=$2 \
               AND aggregate_cursor_item_key IS NOT DISTINCT FROM $3 AND NOT aggregate_complete",
        )
        .bind(request.run_uid)
        .bind(&request.node_id)
        .bind(&current_cursor)
        .bind(&next_cursor)
        .bind(&persisted_output)
        .bind(output_hash)
        .bind(complete)
        .bind(completed_status)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if updated.rows_affected() != 1 {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(MapAggregatePageOutcome::Conflict);
        }
        if complete {
            release_node_dependencies_in_tx(conn.as_mut(), &run, &request.node_id).await?;
        }
        let aggregated_tasks =
            u32::try_from(rows.len()).map_err(|_| Error::ArithmeticOverflow {
                context: "map aggregate page task count".to_string(),
            })?;
        conn.commit().await.map_err(storage_error)?;
        Ok(MapAggregatePageOutcome::Applied {
            next_cursor_item_key: next_cursor,
            aggregated_tasks,
            aggregate_complete: complete,
        })
    }

    /// Loads a constant-size ordinary-progress and terminal-readiness summary.
    pub async fn load_activation_readiness(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionActivationReadiness>> {
        let mut conn = scope.begin(&self.pool).await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_run WHERE run_uid = $1)",
        )
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if !exists {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT \
               EXISTS (SELECT 1 FROM moa.execution_node_state \
                 WHERE run_uid = $1 AND remaining_dependency_count = 0 AND ( \
                   NOT materialization_complete OR (materialization_complete \
                     AND NOT aggregate_complete AND node_status='pending' \
                     AND terminal_task_count=total_task_count AND total_task_count>0)) \
                   AND node_status NOT IN ('completed','skipped','failed','cancelled')) \
                 AS has_actionable_nodes, \
               EXISTS (SELECT 1 FROM moa.execution_node_state \
                 WHERE run_uid = $1 \
                   AND node_status NOT IN ('completed','skipped','failed','cancelled')) \
                 AS has_unfinished_nodes, \
               EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid = $1 \
                 AND status NOT IN \
                   ('completed','skipped','failed','cancelled','unknown_outcome')) \
                 AS has_nonterminal_tasks",
        )
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(Some(ExecutionActivationReadiness {
            has_actionable_nodes: row.try_get("has_actionable_nodes").map_err(row_error)?,
            has_unfinished_nodes: row.try_get("has_unfinished_nodes").map_err(row_error)?,
            has_nonterminal_tasks: row.try_get("has_nonterminal_tasks").map_err(row_error)?,
        }))
    }

    /// Loads one contiguous, byte-bounded prior-round output slice for reduce paging.
    pub async fn load_reduce_round_inputs(
        &self,
        scope: ExecutionScope,
        request: ReduceRoundInputPageRequest,
    ) -> Result<Vec<Value>> {
        let ReduceRoundInputPageRequest {
            run_uid,
            node_id,
            source_round,
            cursor,
            batch_size,
            target_batch_limit,
        } = request;
        if source_round == 0
            || cursor.round != source_round + 1
            || batch_size < 2
            || target_batch_limit == 0
            || target_batch_limit > MAX_READY_PAGE_SIZE
        {
            return Err(Error::InvalidRepositoryInput {
                message: "reduce input page requires adjacent rounds and bounded positive limits"
                    .to_string(),
            });
        }
        let input_offset = cursor
            .batch_cursor
            .checked_mul(u64::from(batch_size))
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "reduce input page offset overflow".to_string(),
            })?;
        let remaining = cursor.round_input_count.saturating_sub(input_offset);
        let input_limit = remaining.min(
            u64::from(target_batch_limit)
                .checked_mul(u64::from(batch_size))
                .ok_or_else(|| Error::InvalidRepositoryInput {
                    message: "reduce input page length overflow".to_string(),
                })?,
        );
        let source_prefix = format!("r{source_round}:b%");
        let input_end =
            input_offset
                .checked_add(input_limit)
                .ok_or_else(|| Error::InvalidRepositoryInput {
                    message: "reduce input page end overflow".to_string(),
                })?;
        let mut conn = scope.begin(&self.pool).await?;
        let bytes = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(pg_column_size(output)), 0)::BIGINT \
             FROM moa.execution_task WHERE run_uid = $1 AND node_id = $2 \
               AND item_key LIKE $3 AND status = 'completed' \
               AND split_part(item_key, ':b', 2)::BIGINT >= $4 \
               AND split_part(item_key, ':b', 2)::BIGINT < $5",
        )
        .bind(run_uid)
        .bind(&node_id)
        .bind(&source_prefix)
        .bind(to_i64(input_offset, "reduce input offset")?)
        .bind(to_i64(input_end, "reduce input end")?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if bytes > MAX_ACTIVATION_OUTPUT_BYTES {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "reduce round input page exceeds {MAX_ACTIVATION_OUTPUT_BYTES} bytes"
                ),
            });
        }
        let rows = sqlx::query_scalar::<_, Value>(
            "SELECT output FROM moa.execution_task \
             WHERE run_uid = $1 AND node_id = $2 AND item_key LIKE $3 \
               AND status = 'completed' \
               AND split_part(item_key, ':b', 2)::BIGINT >= $4 \
               AND split_part(item_key, ':b', 2)::BIGINT < $5 \
             ORDER BY split_part(item_key, ':b', 2)::BIGINT",
        )
        .bind(run_uid)
        .bind(&node_id)
        .bind(source_prefix)
        .bind(to_i64(input_offset, "reduce input offset")?)
        .bind(to_i64(input_end, "reduce input end")?)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        if u64::try_from(rows.len()).map_err(|_| Error::InvalidRepositoryData {
            message: "reduce input page row count does not fit in u64".to_string(),
        })? != input_limit
        {
            return Err(Error::InvalidRepositoryData {
                message: "reduce input page is incomplete or non-contiguous".to_string(),
            });
        }
        Ok(rows)
    }

    /// Creates durable node aggregates and tenant fairness state for one accepted run.
    pub async fn initialize_scheduler_state(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<bool> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(false);
        };
        let run = run_from_row(&run_row)?;
        sqlx::query(
            "INSERT INTO moa.execution_tenant_dispatch_state (tenant_id) VALUES ($1) \
             ON CONFLICT (tenant_id) DO NOTHING",
        )
        .bind(run.tenant_id.0)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;

        let mut inserted_any = false;
        for (node_order, node) in run.active_plan.definition.nodes.iter().enumerate() {
            let node_order =
                i64::try_from(node_order).map_err(|_| Error::InvalidRepositoryInput {
                    message: "execution node order exceeds PostgreSQL BIGINT".to_string(),
                })?;
            let dependency_count = i64::try_from(node.depends_on.len()).map_err(|_| {
                Error::InvalidRepositoryInput {
                    message: "execution dependency count exceeds PostgreSQL BIGINT".to_string(),
                }
            })?;
            let inserted = sqlx::query(
                "INSERT INTO moa.execution_node_state (\
                     node_state_uid, tenant_id, run_uid, node_id, node_order, \
                     dependency_count, remaining_dependency_count\
                 ) VALUES ($1, $2, $3, $4, $5, $6, $6) \
                 ON CONFLICT (run_uid, node_id) DO NOTHING",
            )
            .bind(Uuid::now_v7())
            .bind(run.tenant_id.0)
            .bind(run_uid)
            .bind(&node.id)
            .bind(node_order)
            .bind(dependency_count)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            inserted_any |= inserted.rows_affected() == 1;
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(inserted_any)
    }

    /// Materializes one cursor-fenced page and makes only that bounded page ready.
    pub async fn materialize_ready_page(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: ReadyMaterializationRequest,
    ) -> Result<ReadyMaterializationOutcome> {
        let ReadyMaterializationRequest {
            run_uid,
            plan_revision,
            node_id,
            expected_cursor,
            reduce_cursor,
            source_exhausted,
            terminal_output,
            condition_skipped,
            tasks,
        } = request;
        // A condition skip is the only page that legitimately carries neither tasks nor a
        // terminal output, and it can only ever be the node's first page: the interpreter
        // evaluates `when` at cursor zero and declares the source exhausted there.
        if condition_skipped
            && !(tasks.is_empty()
                && source_exhausted
                && terminal_output.is_none()
                && reduce_cursor.is_none()
                && expected_cursor == 0)
        {
            return Err(Error::InvalidRepositoryInput {
                message: "a condition-skipped page must be the empty exhausted first page"
                    .to_string(),
            });
        }
        if tasks.len() > MAX_READY_PAGE_SIZE_USIZE
            || (!condition_skipped
                && tasks.is_empty()
                && (!source_exhausted || terminal_output.is_none()))
            || (!tasks.is_empty() && terminal_output.is_some())
        {
            return Err(Error::InvalidRepositoryInput {
                message: format!(
                    "ready page must contain 1..={MAX_READY_PAGE_SIZE} tasks or one exhausted source output"
                ),
            });
        }
        if tasks.iter().any(|task| task.node_id != node_id) {
            return Err(Error::InvalidRepositoryInput {
                message: "ready materialization page must contain exactly one node".to_string(),
            });
        }
        let page_count = u64::try_from(tasks.len()).map_err(|_| Error::InvalidRepositoryInput {
            message: "ready materialization page does not fit in u64".to_string(),
        })?;
        let next_cursor = expected_cursor.checked_add(page_count).ok_or_else(|| {
            Error::InvalidRepositoryInput {
                message: "ready materialization cursor overflow".to_string(),
            }
        })?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Conflict);
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != plan_revision
            || !matches!(
                run.status,
                ExecutionRunStatus::Queued
                    | ExecutionRunStatus::Running
                    | ExecutionRunStatus::WaitingInput
                    | ExecutionRunStatus::WaitingReview
                    | ExecutionRunStatus::WaitingSignal
                    | ExecutionRunStatus::WaitingTimer
                    | ExecutionRunStatus::WaitingExternal
                    | ExecutionRunStatus::WaitingReplan
            )
            || run.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Conflict);
        }
        let wait_entered_at =
            sqlx::query_scalar::<_, DateTime<Utc>>("SELECT statement_timestamp()")
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let (storage_wait, wait_deadline_failure) =
            match storage_wait_for_tasks(&tasks, &run, wait_entered_at)? {
                Some(StorageWaitPlan::Enter(wait)) => (Some(*wait), None),
                Some(StorageWaitPlan::DeadlineExceeded(message)) => (None, Some(message)),
                None => (None, None),
            };
        let plan_node = run
            .active_plan
            .definition
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: format!("active plan is missing materialized node `{node_id}`"),
            })?;
        // A skipped branch does NOT reuse the `terminal_output` path: that path validates
        // its output against the node's `output_schema` (which a null branch output cannot
        // satisfy) and lands the node in `completed`. A skip must land in `skipped`, whose
        // null aggregate is what `load_activation_projection` already materializes for
        // dependents and what completion accounting already excludes from requirements.
        if condition_skipped {
            if plan_node.when.is_none() {
                return Err(Error::InvalidRepositoryInput {
                    message: format!("node `{node_id}` was skipped without declaring a condition"),
                });
            }
            return commit_condition_skip(conn, &run, &node_id, expected_cursor).await;
        }
        let reduce_batch_size = match (&plan_node.operation, reduce_cursor) {
            (ExecutionOperation::Reduce { batch_size, .. }, Some(cursor)) => {
                let minimum_inputs = if tasks.is_empty() { 1 } else { 2 };
                if cursor.round == 0 || cursor.round_input_count < minimum_inputs {
                    return Err(Error::InvalidRepositoryInput {
                        message:
                            "reduce cursor requires a one-based round with at least two inputs"
                                .to_string(),
                    });
                }
                Some(u64::from(*batch_size))
            }
            (ExecutionOperation::Reduce { .. }, None) => {
                return Err(Error::InvalidRepositoryInput {
                    message: "reduce ready materialization requires its persisted round cursor"
                        .to_string(),
                });
            }
            (_, Some(_)) => {
                return Err(Error::InvalidRepositoryInput {
                    message: "non-reduce materialization cannot advance a reduce cursor"
                        .to_string(),
                });
            }
            (_, None) => None,
        };
        let cursor_row = sqlx::query(
            "SELECT materialization_cursor, materialization_complete, \
                    aggregate_output, aggregate_output_hash, reduce_round, reduce_batch_cursor, \
                    reduce_round_input_count, reduce_round_task_count, reduce_ready \
             FROM moa.execution_node_state \
             WHERE run_uid = $1 AND node_id = $2 FOR UPDATE",
        )
        .bind(run_uid)
        .bind(&node_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(cursor_row) = cursor_row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Conflict);
        };
        let cursor = required_u64(&cursor_row, "materialization_cursor")?;
        let materialization_complete: bool = cursor_row
            .try_get("materialization_complete")
            .map_err(row_error)?;
        if let Some(output) = terminal_output {
            if cursor != expected_cursor {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ReadyMaterializationOutcome::Conflict);
            }
            validate_instance(
                &plan_node.output_schema,
                &output,
                &format!("node.{}.output", plan_node.id),
            )?;
            let output_bytes = moa_core::canonical_json::canonical_json_bytes(&output)?;
            if output_bytes.len()
                > usize::try_from(MAX_ACTIVATION_OUTPUT_BYTES).map_err(|_| {
                    Error::InvalidRepositoryData {
                        message: "activation output byte ceiling is invalid".to_string(),
                    }
                })?
            {
                return Err(Error::InvalidRepositoryInput {
                    message: format!(
                        "node `{node_id}` aggregate output exceeds {MAX_ACTIVATION_OUTPUT_BYTES} bytes"
                    ),
                });
            }
            let output_hash = node_output_hash(&output)?.to_string();
            if materialization_complete {
                let persisted_output: Option<Value> =
                    cursor_row.try_get("aggregate_output").map_err(row_error)?;
                let persisted_hash: Option<String> = cursor_row
                    .try_get("aggregate_output_hash")
                    .map_err(row_error)?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(
                    if persisted_output.as_ref() == Some(&output)
                        && persisted_hash.as_deref() == Some(output_hash.as_str())
                    {
                        ReadyMaterializationOutcome::Replayed {
                            tasks: Vec::new(),
                            next_cursor: expected_cursor,
                            triggers: Vec::new(),
                        }
                    } else {
                        ReadyMaterializationOutcome::Conflict
                    },
                );
            }
            let (reduce_round, reduce_input_count, reduce_ready) = match (
                &plan_node.operation,
                reduce_cursor,
            ) {
                (ExecutionOperation::Map { .. }, None) => (None, None, false),
                (ExecutionOperation::Reduce { .. }, Some(reduce))
                    if reduce.round == 1
                        && reduce.batch_cursor == 0
                        && reduce.round_input_count == 1 =>
                {
                    let persisted_round = required_u64(&cursor_row, "reduce_round")?;
                    let persisted_batch = required_u64(&cursor_row, "reduce_batch_cursor")?;
                    let persisted_input = optional_u64(&cursor_row, "reduce_round_input_count")?;
                    if persisted_round != 1
                        || persisted_batch != 0
                        || persisted_input.is_some_and(|count| count != 1)
                    {
                        conn.commit().await.map_err(storage_error)?;
                        return Ok(ReadyMaterializationOutcome::Conflict);
                    }
                    (Some(1_u64), Some(1_u64), true)
                }
                _ => {
                    return Err(Error::InvalidRepositoryInput {
                            message: "only an empty map or one-item initial reduce may complete without tasks"
                                .to_string(),
                        });
                }
            };
            let updated = sqlx::query(
                "UPDATE moa.execution_node_state SET node_status = 'completed', \
                     materialization_complete = TRUE, aggregate_output = $4, \
                     aggregate_output_hash = $5, aggregate_complete = TRUE, \
                     reduce_round = COALESCE($6, reduce_round), \
                     reduce_round_input_count = COALESCE($7, reduce_round_input_count), \
                     reduce_ready = $8, updated_at = NOW() \
                 WHERE run_uid = $1 AND node_id = $2 AND materialization_cursor = $3 \
                   AND NOT materialization_complete",
            )
            .bind(run_uid)
            .bind(&node_id)
            .bind(to_i64(
                expected_cursor,
                "expected node materialization cursor",
            )?)
            .bind(&output)
            .bind(&output_hash)
            .bind(
                reduce_round
                    .map(|value| to_i64(value, "reduce round"))
                    .transpose()?,
            )
            .bind(
                reduce_input_count
                    .map(|value| to_i64(value, "reduce input count"))
                    .transpose()?,
            )
            .bind(reduce_ready)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if updated.rows_affected() != 1 {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(ReadyMaterializationOutcome::Conflict);
            }
            release_node_dependents_in_tx(&mut conn, &run, &node_id).await?;
            sqlx::query(
                "UPDATE moa.execution_run SET last_progress_at = NOW(), updated_at = NOW() \
                 WHERE run_uid = $1",
            )
            .bind(run_uid)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Applied {
                tasks: Vec::new(),
                next_cursor: expected_cursor,
                triggers: Vec::new(),
            });
        }
        let task_batch = prepare_task_materialization_batch(run_uid, plan_revision, &tasks)?;
        let next_reduce_cursor = if let (Some(reduce), Some(batch_size)) =
            (reduce_cursor, reduce_batch_size)
        {
            let persisted_round = required_u64(&cursor_row, "reduce_round")?;
            let persisted_batch = required_u64(&cursor_row, "reduce_batch_cursor")?;
            let persisted_input = optional_u64(&cursor_row, "reduce_round_input_count")?;
            let persisted_ready: bool = cursor_row.try_get("reduce_ready").map_err(row_error)?;
            let next_batch = reduce.batch_cursor.checked_add(page_count).ok_or_else(|| {
                Error::InvalidRepositoryInput {
                    message: "reduce batch cursor overflow".to_string(),
                }
            })?;
            let total_batches = reduce.round_input_count.div_ceil(batch_size);
            if source_exhausted != (next_batch == total_batches) {
                return Err(Error::InvalidRepositoryInput {
                    message: "reduce page exhaustion does not match its round cursor".to_string(),
                });
            }
            let expected_persisted_batch = if cursor == next_cursor {
                next_batch
            } else {
                reduce.batch_cursor
            };
            let keys_match = tasks.iter().enumerate().all(|(offset, task)| {
                u64::try_from(offset)
                    .ok()
                    .and_then(|offset| reduce.batch_cursor.checked_add(offset))
                    .is_some_and(|batch| task.item_key == format!("r{}:b{batch}", reduce.round))
            });
            if persisted_round != reduce.round
                || persisted_batch != expected_persisted_batch
                || persisted_input.is_some_and(|count| count != reduce.round_input_count)
                || (cursor == next_cursor && persisted_ready != source_exhausted)
                || (cursor != next_cursor && persisted_ready)
            {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ReadyMaterializationOutcome::Conflict);
            }
            if next_batch > total_batches || !keys_match {
                return Err(Error::InvalidRepositoryInput {
                    message: "reduce page tasks do not match their round/batch cursor".to_string(),
                });
            }
            Some((reduce, next_batch))
        } else {
            None
        };
        if cursor == next_cursor {
            let records = load_and_validate_page(&mut conn, run_uid, &task_batch, &tasks).await?;
            let triggers = load_scheduled_triggers(&mut conn, run_uid, &tasks).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Replayed {
                tasks: records,
                next_cursor,
                triggers,
            });
        }
        if cursor != expected_cursor {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Conflict);
        }

        let inserted = sqlx::query(INSERT_TASK_BATCH_SQL)
            .bind(&task_batch)
            .bind(run_uid)
            .bind(run.tenant_id.0)
            .bind(run.contact_id.map(|value| value.0))
            .bind(to_i64(plan_revision, "plan revision")?)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if inserted.len() != tasks.len() {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReadyMaterializationOutcome::Conflict);
        }
        let task_ids = tasks
            .iter()
            .map(|task| task.task_id.as_uuid())
            .collect::<Vec<_>>();
        let wait_deadline_outcome = wait_deadline_failure.map(|message| {
            crate::state::failed_task_outcome(
                ExecutionFailureClass::DeadlineExceeded,
                message,
                zero_usage(),
            )
        });
        let (task_status, attempt_state, waiting_since, ready_at) =
            match (storage_wait.as_ref(), wait_deadline_outcome.as_ref()) {
                (_, Some(_)) => ("failed", "terminal", None, None),
                (Some(wait), None) => (wait.task_status, "waiting", Some(wait_entered_at), None),
                (None, None) => ("ready", "idle", None, Some(wait_entered_at)),
            };
        let (current_outcome, current_error) = match wait_deadline_outcome.as_ref() {
            Some(outcome) => {
                let (_, error, _) = outcome_projection_fields(outcome)?;
                (Some(serde_json::to_value(outcome)?), error)
            }
            None => (None, None),
        };
        let transitioned = sqlx::query(
            "UPDATE moa.execution_task SET status = $3, attempt_state = $4, \
                 waiting_since = $5, ready_at = $6, \
                 current_outcome = $7, error = $8, \
                 completed_at = CASE WHEN $7::JSONB IS NULL THEN completed_at ELSE NOW() END, \
                 last_progress_at = NOW(), updated_at = NOW() \
             WHERE run_uid = $1 AND task_id = ANY($2::UUID[]) AND status = 'pending'",
        )
        .bind(run_uid)
        .bind(&task_ids)
        .bind(task_status)
        .bind(attempt_state)
        .bind(waiting_since)
        .bind(ready_at)
        .bind(current_outcome)
        .bind(current_error)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if transitioned.rows_affected() != page_count {
            return Err(Error::InvalidRepositoryData {
                message: "inserted ready page did not transition every task".to_string(),
            });
        }
        let ready_delta = if storage_wait.is_some() || wait_deadline_outcome.is_some() {
            0
        } else {
            page_count
        };
        let waiting_delta = if storage_wait.is_some() {
            page_count
        } else {
            0
        };
        // A deadline-failed wait keeps the pre-terminal node status here; the counter
        // transition below flips it to `failed` so dependent cancellation runs once.
        let node_status = match (storage_wait.as_ref(), wait_deadline_outcome.as_ref()) {
            (_, Some(_)) => "pending",
            (Some(_), None) => "waiting",
            (None, None) => "ready",
        };
        let (reduce_round, reduce_batch_cursor, reduce_input_count, reduce_task_delta) =
            next_reduce_cursor.map_or((None, None, None, 0), |(cursor, next_batch)| {
                (
                    Some(cursor.round),
                    Some(next_batch),
                    Some(cursor.round_input_count),
                    page_count,
                )
            });
        sqlx::query(
            "UPDATE moa.execution_node_state \
             SET materialization_cursor = $3, node_status = $6, \
                 materialization_complete = $13, \
                 total_task_count = total_task_count + $4, \
                 ready_task_count = ready_task_count + $7, \
                 waiting_task_count = waiting_task_count + $8, \
                 reduce_round = COALESCE($9, reduce_round), \
                 reduce_batch_cursor = COALESCE($10, reduce_batch_cursor), \
                 reduce_round_input_count = COALESCE($11, reduce_round_input_count), \
                 reduce_round_task_count = reduce_round_task_count + $12, \
                 reduce_ready = CASE WHEN $9::BIGINT IS NULL THEN reduce_ready ELSE $14 END, \
                 updated_at = NOW() \
             WHERE run_uid = $1 AND node_id = $2 AND materialization_cursor = $5",
        )
        .bind(run_uid)
        .bind(&node_id)
        .bind(to_i64(next_cursor, "next node materialization cursor")?)
        .bind(to_i64(page_count, "ready materialization page count")?)
        .bind(to_i64(
            expected_cursor,
            "expected node materialization cursor",
        )?)
        .bind(node_status)
        .bind(to_i64(ready_delta, "ready materialization count")?)
        .bind(to_i64(waiting_delta, "waiting materialization count")?)
        .bind(
            reduce_round
                .map(|value| to_i64(value, "reduce round"))
                .transpose()?,
        )
        .bind(
            reduce_batch_cursor
                .map(|value| to_i64(value, "reduce batch cursor"))
                .transpose()?,
        )
        .bind(
            reduce_input_count
                .map(|value| to_i64(value, "reduce round input count"))
                .transpose()?,
        )
        .bind(to_i64(reduce_task_delta, "reduce round task count")?)
        .bind(source_exhausted)
        .bind(source_exhausted)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let next_wake_at = storage_wait.as_ref().map(|wait| wait.due_at);
        let (waiting_status, waiting_reasons, waiting_since, waiting_truncated) = storage_wait
            .as_ref()
            .map(|wait| {
                let sample_limit = config
                    .maximum_activation_steps
                    .min(MAX_WAITING_REASON_SAMPLES);
                let reasons = bounded_waiting_reason_sample(
                    &run.waiting_reasons,
                    &wait.reason,
                    sample_limit,
                )?;
                let waiting_count = run.waiting_task_count.checked_add(1).ok_or_else(|| {
                    Error::ArithmeticOverflow {
                        context: "run waiting task count".to_string(),
                    }
                })?;
                let sample_count =
                    u64::try_from(reasons.len()).map_err(|_| Error::ArithmeticOverflow {
                        context: "run waiting reason sample count".to_string(),
                    })?;
                Ok::<_, Error>((
                    Some(waiting_run_status_after(&run, wait.task_status).as_str()),
                    Some(serde_json::to_value(reasons)?),
                    Some(run.waiting_since.unwrap_or(wait_entered_at)),
                    Some(waiting_count > sample_count),
                ))
            })
            .transpose()?
            .unwrap_or((None, None, None, None));
        let run_waiting = storage_wait.as_ref().map_or_else(
            || Ok(RunWaitingCounterDelta::default()),
            |wait| {
                run_waiting_counter_delta(
                    ExecutionTaskStatus::Pending,
                    storage_wait_task_status(wait.task_status)?,
                    None,
                )
            },
        )?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET progress_total_tasks = progress_total_tasks + $2, \
                 ready_task_count = ready_task_count + $3, \
                 next_wake_at = CASE WHEN $4::TIMESTAMPTZ IS NULL THEN next_wake_at \
                     WHEN next_wake_at IS NULL THEN $4 ELSE LEAST(next_wake_at, $4) END, \
                 status = COALESCE($5, status), \
                 waiting_reasons = COALESCE($6, waiting_reasons), \
                 waiting_since = CASE WHEN $6::JSONB IS NULL THEN waiting_since \
                     ELSE COALESCE(waiting_since, $7) END, \
                 waiting_reasons_truncated = COALESCE($8, waiting_reasons_truncated), \
                 waiting_task_count = waiting_task_count + $9, \
                 waiting_review_task_count = waiting_review_task_count + $10, \
                 waiting_signal_task_count = waiting_signal_task_count + $11, \
                 waiting_timer_task_count = waiting_timer_task_count + $12, \
                 progress_failed_tasks = progress_failed_tasks + $13, \
                 last_progress_at = NOW(), updated_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .bind(to_i64(page_count, "ready materialization page count")?)
        .bind(to_i64(ready_delta, "ready materialization count")?)
        .bind(next_wake_at)
        .bind(waiting_status)
        .bind(waiting_reasons)
        .bind(waiting_since)
        .bind(waiting_truncated)
        .bind(run_waiting.total)
        .bind(run_waiting.review)
        .bind(run_waiting.signal)
        .bind(run_waiting.timer)
        .bind(to_i64(
            u64::from(wait_deadline_outcome.is_some()),
            "wait deadline failure count",
        )?)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if wait_deadline_outcome.is_some() {
            let task = &tasks[0];
            transition_node_counters_in_tx(
                &mut conn,
                run_uid,
                &node_id,
                &task.item_key,
                ExecutionTaskStatus::Pending,
                ExecutionTaskStatus::Failed,
            )
            .await?;
        }
        let mut triggers = Vec::new();
        if let Some(wait) = storage_wait {
            let task = &tasks[0];
            let write = create_trigger_with_dispatch_in_conn(
                conn.as_mut(),
                config,
                &NewExecutionTrigger {
                    trigger_uid: storage_wait_trigger_uid(
                        task.task_id,
                        task.generation,
                        wait.trigger_kind,
                    ),
                    tenant_id: run.tenant_id,
                    run_uid: Some(run_uid),
                    task_id: Some(task.task_id.as_uuid()),
                    compensation_id: None,
                    schedule_uid: None,
                    kind: wait.trigger_kind,
                    controller_generation: Some(run.controller_generation),
                    attempt_generation: Some(task.generation),
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    schedule_incarnation: None,
                    occurrence_sequence: None,
                    due_at: wait.due_at,
                    payload: json!({ "task_id": task.task_id }),
                },
            )
            .await?;
            triggers.push(ExecutionScheduledTrigger {
                dispatch_uid: write.dispatch.dispatch_uid,
                tenant_id: run.tenant_id,
                trigger_uid: write.trigger.trigger_uid,
                due_at: wait.due_at,
            });
        }
        let records = load_and_validate_page(&mut conn, run_uid, &task_batch, &tasks).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ReadyMaterializationOutcome::Applied {
            tasks: records,
            next_cursor,
            triggers,
        })
    }

    /// Loads only exact referenced terminal outputs, with a hard request-size bound.
    pub async fn load_referenced_task_outputs(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_ids: &[ExecutionTaskId],
    ) -> Result<BTreeMap<ExecutionTaskId, Value>> {
        if task_ids.len() > MAX_READY_PAGE_SIZE_USIZE {
            return Err(Error::InvalidRepositoryInput {
                message: format!("referenced output load exceeds {MAX_READY_PAGE_SIZE} tasks"),
            });
        }
        let ids = task_ids.iter().map(|id| id.as_uuid()).collect::<Vec<_>>();
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT task_id, output FROM moa.execution_task \
             WHERE run_uid = $1 AND task_id = ANY($2::UUID[]) \
               AND status IN ('completed', 'skipped') ORDER BY task_id",
        )
        .bind(run_uid)
        .bind(ids)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        rows.into_iter()
            .map(|row| {
                let task_id: Uuid = row.try_get("task_id").map_err(row_error)?;
                let output: Value = row.try_get("output").map_err(row_error)?;
                Ok((ExecutionTaskId::from_uuid(task_id), output))
            })
            .collect()
    }

    /// Scans one bounded task page while proving terminality without an unbounded load.
    pub async fn load_terminal_verification_page(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        after: Option<ExecutionTaskId>,
        limit: u32,
    ) -> Result<ExecutionTerminalVerificationPage> {
        let limit = limit.clamp(1, MAX_READY_PAGE_SIZE);
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            "SELECT * FROM moa.execution_task WHERE run_uid = $1 \
               AND ($2::UUID IS NULL OR task_id > $2) \
             ORDER BY task_id LIMIT $3",
        )
        .bind(run_uid)
        .bind(after.map(ExecutionTaskId::as_uuid))
        .bind(i64::from(limit))
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let tasks = rows.iter().map(task_from_row).collect::<Result<Vec<_>>>()?;
        let limit = usize::try_from(limit).map_err(|_| Error::InvalidRepositoryInput {
            message: "terminal verification page does not fit in memory".to_string(),
        })?;
        let next_cursor = (tasks.len() == limit)
            .then(|| tasks.last().map(|task| task.task_id))
            .flatten();
        let nonterminal_tasks = tasks
            .into_iter()
            .filter(|task| !task.status.is_terminal())
            .collect();
        Ok(ExecutionTerminalVerificationPage {
            nonterminal_tasks,
            next_cursor,
        })
    }

    /// Builds compact terminal Session delivery evidence with SQL-enforced row limits.
    pub async fn load_bounded_terminal_delivery(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionTerminalDelivery>> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let run = run_from_row(&run_row)?;
        if !run.status.is_terminal() {
            return Err(Error::InvalidRepositoryData {
                message: format!("execution run `{run_uid}` is not terminal"),
            });
        }
        let citation_limit = i64::try_from(crate::wire::EXECUTION_TERMINAL_MAX_CITATION_IDS)
            .map_err(|_| Error::InvalidRepositoryData {
                message: "terminal citation limit exceeds PostgreSQL BIGINT".to_string(),
            })?;
        let failure_limit =
            i64::try_from(crate::wire::EXECUTION_TERMINAL_MAX_FAILURES).map_err(|_| {
                Error::InvalidRepositoryData {
                    message: "terminal failure limit exceeds PostgreSQL BIGINT".to_string(),
                }
            })?;
        let citation_ids = sqlx::query_scalar::<_, String>(
            "SELECT citation.value ->> 'source_id' \
             FROM moa.execution_task task \
             CROSS JOIN LATERAL jsonb_array_elements(task.citations) \
               WITH ORDINALITY AS citation(value, position) \
             WHERE task.run_uid = $1 \
               AND NULLIF(btrim(citation.value ->> 'source_id'), '') IS NOT NULL \
             ORDER BY task.node_id, task.item_key, task.task_id, citation.position \
             LIMIT $2",
        )
        .bind(run_uid)
        .bind(citation_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let failures = sqlx::query_scalar::<_, String>(
            "SELECT COALESCE(error, current_outcome #>> '{result,message}') \
             FROM moa.execution_task WHERE run_uid = $1 \
               AND status IN ('failed', 'unknown_outcome') \
               AND COALESCE(error, current_outcome #>> '{result,message}') IS NOT NULL \
             ORDER BY node_id, item_key, task_id LIMIT $2",
        )
        .bind(run_uid)
        .bind(failure_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let summary = crate::wire::build_execution_terminal_summary(
            run.run_uid,
            run.originating_user_sequence_num,
            run.output.as_ref(),
            citation_ids,
            failures,
            run.terminal_gaps.clone(),
        )?;
        Ok(Some(ExecutionTerminalDelivery {
            status: run.status,
            summary,
        }))
    }
}

fn storage_wait_trigger_uid(
    task_id: ExecutionTaskId,
    generation: u64,
    kind: ExecutionTriggerKind,
) -> Uuid {
    Uuid::new_v5(
        &task_id.as_uuid(),
        format!(
            "execution-storage-wait-trigger-v1:{generation}:{}",
            kind.as_str()
        )
        .as_bytes(),
    )
}

fn node_state_from_row(row: &PgRow) -> Result<ExecutionNodeStateRecord> {
    let status: String = row.try_get("node_status").map_err(row_error)?;
    let status = match status.as_str() {
        "pending" => ExecutionNodeQueueStatus::Pending,
        "ready" => ExecutionNodeQueueStatus::Ready,
        "running" => ExecutionNodeQueueStatus::Running,
        "waiting" => ExecutionNodeQueueStatus::Waiting,
        "completed" => ExecutionNodeQueueStatus::Completed,
        "skipped" => ExecutionNodeQueueStatus::Skipped,
        "failed" => ExecutionNodeQueueStatus::Failed,
        "cancelled" => ExecutionNodeQueueStatus::Cancelled,
        other => {
            return Err(Error::InvalidRepositoryData {
                message: format!("unknown execution node queue status `{other}`"),
            });
        }
    };
    let aggregate_output: Option<Value> = row.try_get("aggregate_output").map_err(row_error)?;
    let aggregate_output_hash: Option<String> =
        row.try_get("aggregate_output_hash").map_err(row_error)?;
    let aggregate_output_hash = aggregate_output_hash
        .map(|hash| ExecutionHash::from_str(&hash))
        .transpose()?;
    if aggregate_output.is_some() != aggregate_output_hash.is_some() {
        return Err(Error::InvalidRepositoryData {
            message: "node aggregate output/hash pair is incomplete".to_string(),
        });
    }
    if let (Some(output), Some(expected_hash)) = (&aggregate_output, aggregate_output_hash)
        && node_output_hash(output)? != expected_hash
    {
        return Err(Error::InvalidRepositoryData {
            message: "node aggregate output hash does not match canonical bytes".to_string(),
        });
    }
    Ok(ExecutionNodeStateRecord {
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        node_id: row.try_get("node_id").map_err(row_error)?,
        node_order: required_u64(row, "node_order")?,
        status,
        materialization_cursor: required_u64(row, "materialization_cursor")?,
        materialization_complete: row.try_get("materialization_complete").map_err(row_error)?,
        reduce_round: required_u64(row, "reduce_round")?,
        reduce_batch_cursor: required_u64(row, "reduce_batch_cursor")?,
        reduce_round_input_count: optional_u64(row, "reduce_round_input_count")?,
        reduce_round_task_count: required_u64(row, "reduce_round_task_count")?,
        reduce_round_terminal_task_count: required_u64(row, "reduce_round_terminal_task_count")?,
        reduce_ready: row.try_get("reduce_ready").map_err(row_error)?,
        remaining_dependency_count: required_u64(row, "remaining_dependency_count")?,
        total_task_count: required_u64(row, "total_task_count")?,
        ready_task_count: required_u64(row, "ready_task_count")?,
        active_task_count: required_u64(row, "active_task_count")?,
        waiting_task_count: required_u64(row, "waiting_task_count")?,
        terminal_task_count: required_u64(row, "terminal_task_count")?,
        aggregate_output,
        aggregate_output_hash,
        aggregate_cursor_item_key: row
            .try_get("aggregate_cursor_item_key")
            .map_err(row_error)?,
        aggregate_complete: row.try_get("aggregate_complete").map_err(row_error)?,
    })
}

struct StorageWaitMaterialization {
    task_status: &'static str,
    trigger_kind: ExecutionTriggerKind,
    due_at: DateTime<Utc>,
    reason: WaitingReason,
}

const fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

/// How one storage-only wait node materializes at wait entry.
enum StorageWaitPlan {
    /// The wait is enterable and parks the task on its durable trigger.
    Enter(Box<StorageWaitMaterialization>),
    /// The wait cannot finish before the run deadline and fails the task instead.
    DeadlineExceeded(String),
}

fn storage_wait_for_tasks(
    tasks: &[LogicalTask],
    run: &ExecutionRunRecord,
    wait_entered_at: DateTime<Utc>,
) -> Result<Option<StorageWaitPlan>> {
    let Some(first) = tasks.first() else {
        return Ok(None);
    };
    let (task_status, trigger_kind, target) = match &first.kind {
        LogicalTaskKind::Review { wait_policy, .. } => (
            "waiting_review",
            ExecutionTriggerKind::WaitExpiry,
            &wait_policy.expiry,
        ),
        LogicalTaskKind::WaitSignal { wait_policy, .. } => (
            "waiting_signal",
            ExecutionTriggerKind::WaitExpiry,
            &wait_policy.expiry,
        ),
        LogicalTaskKind::WaitUntil { wake, .. } => {
            ("waiting_timer", ExecutionTriggerKind::TaskTimer, wake)
        }
        _ => return Ok(None),
    };
    if tasks.len() != 1 {
        return Err(Error::InvalidRepositoryInput {
            message: "storage-only wait nodes must materialize exactly one logical task"
                .to_string(),
        });
    }
    let run_deadline_at =
        run.approved_budget
            .deadline_at
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "storage-only waits require an absolute run deadline".to_string(),
            })?;
    let due_at = match crate::interpreter::resolve_temporal_target_within_deadline(
        target,
        wait_entered_at,
        run_deadline_at,
    )? {
        TemporalTargetResolution::Due(due_at) => due_at,
        TemporalTargetResolution::DeadlineExceeded {
            due_at,
            run_deadline_at,
        } => {
            return Ok(Some(StorageWaitPlan::DeadlineExceeded(format!(
                "wait on node `{}` entered at {wait_entered_at} resolves at {due_at}, \
                 at or after the run deadline {run_deadline_at}",
                first.node_id
            ))));
        }
    };
    let exact_target = ExecutionTemporalTarget::At { at: due_at };
    let reason = match &first.kind {
        LogicalTaskKind::Review {
            prompt,
            wait_policy,
        } => WaitingReason::Review {
            task_id: first.task_id,
            prompt: prompt.clone(),
            wait_policy: ExecutionWaitPolicy {
                expiry: exact_target,
                on_expiry: wait_policy.on_expiry.clone(),
            },
        },
        LogicalTaskKind::WaitSignal {
            signal_name,
            wait_policy,
        } => WaitingReason::Signal {
            task_id: first.task_id,
            signal_name: signal_name.clone(),
            wait_policy: ExecutionWaitPolicy {
                expiry: exact_target,
                on_expiry: wait_policy.on_expiry.clone(),
            },
        },
        LogicalTaskKind::WaitUntil { .. } => WaitingReason::Timer {
            task_id: first.task_id,
            wake: exact_target,
        },
        LogicalTaskKind::Capability { .. }
        | LogicalTaskKind::Agent { .. }
        | LogicalTaskKind::Output { .. }
        | LogicalTaskKind::CompletionVerifier { .. } => {
            return Err(Error::InvalidRepositoryData {
                message: "compute task reached storage-wait materialization".to_string(),
            });
        }
    };
    Ok(Some(StorageWaitPlan::Enter(Box::new(
        StorageWaitMaterialization {
            task_status,
            trigger_kind,
            due_at,
            reason,
        },
    ))))
}

fn waiting_reason_task_id(reason: &WaitingReason) -> Option<ExecutionTaskId> {
    match reason {
        WaitingReason::Input { task_id, .. }
        | WaitingReason::Review { task_id, .. }
        | WaitingReason::Signal { task_id, .. }
        | WaitingReason::Timer { task_id, .. }
        | WaitingReason::External { task_id } => Some(*task_id),
        WaitingReason::RunningTasks | WaitingReason::Dependencies { .. } => None,
    }
}

fn bounded_waiting_reason_sample(
    existing: &[WaitingReason],
    inserted: &WaitingReason,
    limit: usize,
) -> Result<Vec<WaitingReason>> {
    let mut candidates = existing
        .iter()
        .filter(|reason| waiting_reason_task_id(reason).is_some())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(inserted_task_id) = waiting_reason_task_id(inserted)
        && !candidates
            .iter()
            .any(|reason| waiting_reason_task_id(reason) == Some(inserted_task_id))
    {
        candidates.push(inserted.clone());
    }
    candidates.sort_by_key(waiting_reason_task_id);
    let mut sample = Vec::new();
    for reason in candidates {
        if sample.len() >= limit {
            break;
        }
        sample.push(reason);
        if serde_json::to_vec(&sample)?.len() > MAX_WAITING_REASON_SAMPLE_BYTES {
            sample.pop();
        }
    }
    Ok(sample)
}

pub(super) async fn append_run_wait_reason_in_tx(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    reason: &WaitingReason,
    entered_at: DateTime<Utc>,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT waiting_reasons, waiting_task_count, waiting_since \
         FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE",
    )
    .bind(run_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "waiting task references a missing execution run".to_string(),
    })?;
    let existing: Vec<WaitingReason> =
        serde_json::from_value(row.try_get("waiting_reasons").map_err(row_error)?)?;
    let waiting_task_count = required_u64(&row, "waiting_task_count")?;
    let waiting_since: Option<DateTime<Utc>> = row.try_get("waiting_since").map_err(row_error)?;
    let reasons = bounded_waiting_reason_sample(&existing, reason, MAX_WAITING_REASON_SAMPLES)?;
    let sample_count = u64::try_from(reasons.len()).map_err(|_| Error::ArithmeticOverflow {
        context: "run waiting reason sample count".to_string(),
    })?;
    sqlx::query(
        "UPDATE moa.execution_run SET waiting_reasons=$2, \
         waiting_reasons_truncated=$3, waiting_since=$4, \
         status=CASE \
             WHEN status IN ('pause_requested','pausing','paused') THEN status \
             WHEN waiting_input_task_count > 0 THEN 'waiting_input' \
             WHEN waiting_review_task_count > 0 THEN 'waiting_review' \
             WHEN waiting_signal_task_count > 0 THEN 'waiting_signal' \
             WHEN waiting_timer_task_count > 0 THEN 'waiting_timer' \
             WHEN waiting_external_task_count > 0 THEN 'waiting_external' \
             WHEN waiting_replan_task_count > 0 THEN 'waiting_replan' \
             ELSE 'running' END, \
         last_progress_at=GREATEST(last_progress_at,$5), updated_at=NOW() \
         WHERE run_uid=$1",
    )
    .bind(run_uid)
    .bind(serde_json::to_value(reasons)?)
    .bind(sample_count < waiting_task_count)
    .bind(waiting_since.unwrap_or(entered_at))
    .bind(entered_at)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

fn storage_wait_task_status(status: &str) -> Result<ExecutionTaskStatus> {
    match status {
        "waiting_review" => Ok(ExecutionTaskStatus::WaitingReview),
        "waiting_signal" => Ok(ExecutionTaskStatus::WaitingSignal),
        "waiting_timer" => Ok(ExecutionTaskStatus::WaitingTimer),
        other => Err(Error::InvalidRepositoryData {
            message: format!("unknown storage wait task status `{other}`"),
        }),
    }
}

fn waiting_run_status_after(
    run: &ExecutionRunRecord,
    inserted_task_status: &str,
) -> ExecutionRunStatus {
    let inserted_review = u64::from(inserted_task_status == "waiting_review");
    let inserted_signal = u64::from(inserted_task_status == "waiting_signal");
    let inserted_timer = u64::from(inserted_task_status == "waiting_timer");
    if run.waiting_input_task_count > 0 {
        ExecutionRunStatus::WaitingInput
    } else if run
        .waiting_review_task_count
        .saturating_add(inserted_review)
        > 0
    {
        ExecutionRunStatus::WaitingReview
    } else if run
        .waiting_signal_task_count
        .saturating_add(inserted_signal)
        > 0
    {
        ExecutionRunStatus::WaitingSignal
    } else if run.waiting_timer_task_count.saturating_add(inserted_timer) > 0 {
        ExecutionRunStatus::WaitingTimer
    } else if run.waiting_external_task_count > 0 {
        ExecutionRunStatus::WaitingExternal
    } else if run.waiting_replan_task_count > 0 {
        ExecutionRunStatus::WaitingReplan
    } else {
        ExecutionRunStatus::Running
    }
}

/// Commits the one durable effect of a false node condition: a `skipped` node aggregate.
///
/// The node keeps zero logical tasks forever, so its aggregate output is JSON `null`
/// with a verified hash and `aggregate_complete`, exactly the shape dependents already
/// load. Dependents are then released so ordering-only successors of a skipped branch
/// still run.
async fn commit_condition_skip(
    mut conn: ScopedConn<'_>,
    run: &ExecutionRunRecord,
    node_id: &str,
    expected_cursor: u64,
) -> Result<ReadyMaterializationOutcome> {
    let Some(row) = sqlx::query(
        "SELECT node_status, materialization_cursor, materialization_complete, \
                total_task_count, aggregate_complete \
         FROM moa.execution_node_state WHERE run_uid = $1 AND node_id = $2 FOR UPDATE",
    )
    .bind(run.run_uid)
    .bind(node_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    else {
        conn.commit().await.map_err(storage_error)?;
        return Ok(ReadyMaterializationOutcome::Conflict);
    };
    let status: String = row.try_get("node_status").map_err(row_error)?;
    let cursor = required_u64(&row, "materialization_cursor")?;
    let total_task_count = required_u64(&row, "total_task_count")?;
    let materialization_complete: bool =
        row.try_get("materialization_complete").map_err(row_error)?;
    let aggregate_complete: bool = row.try_get("aggregate_complete").map_err(row_error)?;
    // `skipped` is also reachable for a node whose tasks all settled without succeeding,
    // so the zero-task shape is what distinguishes an already-committed condition skip
    // from that unrelated aggregate.
    if status == "skipped"
        && total_task_count == 0
        && materialization_complete
        && aggregate_complete
    {
        conn.commit().await.map_err(storage_error)?;
        return Ok(ReadyMaterializationOutcome::Replayed {
            tasks: Vec::new(),
            next_cursor: expected_cursor,
            triggers: Vec::new(),
        });
    }
    if cursor != expected_cursor
        || materialization_complete
        || total_task_count != 0
        || status != "pending"
    {
        conn.commit().await.map_err(storage_error)?;
        return Ok(ReadyMaterializationOutcome::Conflict);
    }
    let output_hash = node_output_hash(&Value::Null)?.to_string();
    let updated = sqlx::query(
        "UPDATE moa.execution_node_state SET node_status = 'skipped', \
             materialization_complete = TRUE, aggregate_output = 'null'::JSONB, \
             aggregate_output_hash = $4, aggregate_complete = TRUE, updated_at = NOW() \
         WHERE run_uid = $1 AND node_id = $2 AND materialization_cursor = $3 \
           AND NOT materialization_complete AND node_status = 'pending' \
           AND total_task_count = 0",
    )
    .bind(run.run_uid)
    .bind(node_id)
    .bind(to_i64(
        expected_cursor,
        "expected node materialization cursor",
    )?)
    .bind(&output_hash)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if updated.rows_affected() != 1 {
        conn.rollback().await.map_err(storage_error)?;
        return Ok(ReadyMaterializationOutcome::Conflict);
    }
    release_node_dependents_in_tx(&mut conn, run, node_id).await?;
    sqlx::query(
        "UPDATE moa.execution_run SET last_progress_at = NOW(), updated_at = NOW() \
         WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    conn.commit().await.map_err(storage_error)?;
    Ok(ReadyMaterializationOutcome::Applied {
        tasks: Vec::new(),
        next_cursor: expected_cursor,
        triggers: Vec::new(),
    })
}

/// Releases one dependency edge on every direct dependent of a settled node.
///
/// A dependent whose counter cannot be decremented is normally a corrupted projection,
/// with one legitimate exception: a *sibling* dependency may have failed first and
/// cancelled this dependent through `cancel_unmaterialized_dependents_in_tx`, which
/// zeroes its counter. That is reachable whenever one branch of a fan-in settles
/// without tasks — an empty map, or a node whose condition evaluated false — after a
/// sibling has already failed.
async fn release_node_dependents_in_tx(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    node_id: &str,
) -> Result<()> {
    for dependent in run.active_plan.definition.nodes.iter().filter(|node| {
        node.depends_on
            .iter()
            .any(|dependency| dependency == node_id)
    }) {
        let released = sqlx::query(
            "UPDATE moa.execution_node_state \
             SET remaining_dependency_count = remaining_dependency_count - 1, \
                 updated_at = NOW() \
             WHERE run_uid = $1 AND node_id = $2 AND remaining_dependency_count > 0",
        )
        .bind(run.run_uid)
        .bind(&dependent.id)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if released.rows_affected() != 1 {
            let already_cancelled = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM moa.execution_node_state \
                 WHERE run_uid = $1 AND node_id = $2 AND node_status = 'cancelled' \
                   AND total_task_count = 0 AND remaining_dependency_count = 0 \
                   AND materialization_complete AND aggregate_complete)",
            )
            .bind(run.run_uid)
            .bind(&dependent.id)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if already_cancelled {
                continue;
            }
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "dependent node `{}` lost its dependency counter",
                    dependent.id
                ),
            });
        }
    }
    Ok(())
}

async fn load_scheduled_triggers(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    tasks: &[LogicalTask],
) -> Result<Vec<ExecutionScheduledTrigger>> {
    let task_ids = tasks
        .iter()
        .map(|task| task.task_id.as_uuid())
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "SELECT dispatch.dispatch_uid, trigger.tenant_id, trigger.trigger_uid, trigger.due_at \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.tenant_id = trigger.tenant_id \
          AND dispatch.trigger_uid = trigger.trigger_uid \
          AND dispatch.dispatch_kind = 'trigger_delivery' \
         WHERE trigger.run_uid = $1 AND trigger.task_id = ANY($2::UUID[]) \
           AND trigger.trigger_kind IN ('task_timer', 'wait_expiry') \
         ORDER BY trigger.task_id, trigger.trigger_uid",
    )
    .bind(run_uid)
    .bind(task_ids)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            Ok(ExecutionScheduledTrigger {
                dispatch_uid: row.try_get("dispatch_uid").map_err(row_error)?,
                tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id").map_err(row_error)?),
                trigger_uid: row.try_get("trigger_uid").map_err(row_error)?,
                due_at: row.try_get("due_at").map_err(row_error)?,
            })
        })
        .collect()
}

async fn load_and_validate_page(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    task_batch: &Value,
    requested: &[LogicalTask],
) -> Result<Vec<ExecutionTaskRecord>> {
    let rows = sqlx::query(LOAD_TASK_BATCH_SQL)
        .bind(task_batch)
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    if rows.len() != requested.len() {
        return Err(Error::InvalidRepositoryData {
            message: "ready materialization replay did not reload the exact page".to_string(),
        });
    }
    rows.iter()
        .zip(requested)
        .map(|(row, requested)| {
            let record = task_from_row(row)?;
            ensure_materialization_replay_matches(&record, requested)?;
            if record.status != ExecutionTaskStatus::Ready
                && record.status != ExecutionTaskStatus::Dispatching
                && record.status != ExecutionTaskStatus::Running
                && record.status != ExecutionTaskStatus::WaitingReview
                && record.status != ExecutionTaskStatus::WaitingSignal
                && record.status != ExecutionTaskStatus::WaitingTimer
                && !record.status.is_terminal()
            {
                return Err(Error::InvalidRepositoryData {
                    message: "replayed ready page contains a task outside its lifecycle"
                        .to_string(),
                });
            }
            Ok(record)
        })
        .collect()
}

/// Updates task-derived node/run counters in the caller's canonical mutation transaction.
pub(super) async fn transition_node_counters_in_tx(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    from: ExecutionTaskStatus,
    to: ExecutionTaskStatus,
) -> Result<()> {
    transition_node_counters_inner(conn, run_uid, node_id, item_key, from, to, None).await
}

/// Updates counters for one transition into or out of an audience-qualified input wait.
pub(super) async fn transition_node_counters_with_input_audience_in_tx(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    from: ExecutionTaskStatus,
    to: ExecutionTaskStatus,
    input_audience: &InputAudience,
) -> Result<()> {
    transition_node_counters_inner(
        conn,
        run_uid,
        node_id,
        item_key,
        from,
        to,
        Some(input_audience),
    )
    .await
}

async fn transition_node_counters_inner(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    from: ExecutionTaskStatus,
    to: ExecutionTaskStatus,
    input_audience: Option<&InputAudience>,
) -> Result<()> {
    if from == to {
        return Ok(());
    }
    let touches_input =
        from == ExecutionTaskStatus::WaitingInput || to == ExecutionTaskStatus::WaitingInput;
    if touches_input != input_audience.is_some() {
        return Err(Error::InvalidRepositoryInput {
            message: "WaitingInput counter transitions require exactly one typed input audience"
                .to_string(),
        });
    }
    let run_row = sqlx::query(LOAD_RUN_SQL)
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let run = run_from_row(&run_row)?;
    let node = run
        .active_plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == node_id);
    let is_verifier = node_id.starts_with("@check/");
    if node.is_none() && !is_verifier {
        return Err(Error::InvalidRepositoryData {
            message: format!("active plan is missing transitioned node `{node_id}`"),
        });
    }
    let is_reduce =
        node.is_some_and(|node| matches!(node.operation, ExecutionOperation::Reduce { .. }));
    let is_map = node.is_some_and(|node| matches!(node.operation, ExecutionOperation::Map { .. }));
    let previous_node = sqlx::query(
        "SELECT node_status, reduce_round, reduce_round_task_count, \
                reduce_round_terminal_task_count, reduce_ready \
         FROM moa.execution_node_state \
         WHERE run_uid = $1 AND node_id = $2 FOR UPDATE",
    )
    .bind(run_uid)
    .bind(node_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: format!("missing node counters for `{node_id}`"),
    })?;
    let previous_node_status: String = previous_node.try_get("node_status").map_err(row_error)?;
    let from_counts = task_counter_class(from);
    let to_counts = task_counter_class(to);
    let ready_delta = to_counts.ready - from_counts.ready;
    let active_delta = to_counts.active - from_counts.active;
    let waiting_delta = to_counts.waiting - from_counts.waiting;
    let terminal_delta = to_counts.terminal - from_counts.terminal;
    let succeeded_delta = to_counts.succeeded - from_counts.succeeded;
    let failed_delta = to_counts.failed - from_counts.failed;
    let cancelled_delta = to_counts.cancelled - from_counts.cancelled;
    let run_waiting = run_waiting_counter_delta(from, to, input_audience)?;
    let reduce_terminal_delta = if is_reduce && terminal_delta != 0 {
        let task_round = reduce_round_from_item_key(item_key)?;
        let current_round = required_u64(&previous_node, "reduce_round")?;
        if task_round != current_round {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "reduce task round {task_round} does not match current round {current_round}"
                ),
            });
        }
        terminal_delta
    } else {
        0
    };
    let updated = sqlx::query(
        "UPDATE moa.execution_node_state SET \
             ready_task_count = ready_task_count + $3, \
             active_task_count = active_task_count + $4, \
             waiting_task_count = waiting_task_count + $5, \
             terminal_task_count = terminal_task_count + $6, \
             succeeded_task_count = succeeded_task_count + $7, \
             failed_task_count = failed_task_count + $8, \
             cancelled_task_count = cancelled_task_count + $9, \
             reduce_round_terminal_task_count = \
                 reduce_round_terminal_task_count + $10, \
             node_status = CASE \
                 WHEN failed_task_count + $8 > 0 THEN 'failed' \
                 WHEN cancelled_task_count + $9 > 0 THEN 'cancelled' \
                 WHEN $11 AND reduce_ready \
                      AND reduce_round_terminal_task_count + $10 = reduce_round_task_count \
                      AND reduce_round_task_count = 1 THEN 'completed' \
                 WHEN $11 AND reduce_ready \
                      AND reduce_round_terminal_task_count + $10 = reduce_round_task_count \
                      AND reduce_round_task_count > 1 THEN 'pending' \
                 WHEN $12 AND materialization_complete \
                      AND terminal_task_count + $6 = total_task_count \
                      AND total_task_count > 0 THEN 'pending' \
                 WHEN NOT $11 AND materialization_complete \
                      AND terminal_task_count + $6 = total_task_count \
                      AND total_task_count > 0 THEN \
                     CASE WHEN succeeded_task_count + $7 = 0 THEN 'skipped' ELSE 'completed' END \
                 WHEN waiting_task_count + $5 > 0 THEN 'waiting' \
                 WHEN active_task_count + $4 > 0 THEN 'running' \
                 WHEN ready_task_count + $3 > 0 THEN 'ready' \
                 ELSE 'pending' END, \
             updated_at = NOW() \
         WHERE run_uid = $1 AND node_id = $2 \
           AND ready_task_count + $3 >= 0 AND active_task_count + $4 >= 0 \
           AND waiting_task_count + $5 >= 0 AND terminal_task_count + $6 >= 0 \
           AND reduce_round_terminal_task_count + $10 >= 0 \
         RETURNING node_status, reduce_round, reduce_round_task_count, \
                   reduce_round_terminal_task_count, reduce_ready",
    )
    .bind(run_uid)
    .bind(node_id)
    .bind(ready_delta)
    .bind(active_delta)
    .bind(waiting_delta)
    .bind(terminal_delta)
    .bind(succeeded_delta)
    .bind(failed_delta)
    .bind(cancelled_delta)
    .bind(reduce_terminal_delta)
    .bind(is_reduce)
    .bind(is_map)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(updated) = updated else {
        return Err(Error::InvalidRepositoryData {
            message: format!("missing or inconsistent node counters for `{node_id}`"),
        });
    };
    let mut updated_node_status: String = updated.try_get("node_status").map_err(row_error)?;
    if is_reduce {
        let round_task_count = required_u64(&updated, "reduce_round_task_count")?;
        let round_terminal_count = required_u64(&updated, "reduce_round_terminal_task_count")?;
        let round_ready: bool = updated.try_get("reduce_ready").map_err(row_error)?;
        if round_ready && round_task_count > 1 && round_terminal_count == round_task_count {
            let advanced = sqlx::query(
                "UPDATE moa.execution_node_state \
                 SET reduce_round = reduce_round + 1, reduce_batch_cursor = 0, \
                     reduce_round_input_count = $3, reduce_round_task_count = 0, \
                     reduce_round_terminal_task_count = 0, reduce_ready = FALSE, \
                     materialization_complete = FALSE, node_status = 'pending', \
                     updated_at = NOW() \
                 WHERE run_uid = $1 AND node_id = $2 AND reduce_round = $4",
            )
            .bind(run_uid)
            .bind(node_id)
            .bind(to_i64(round_task_count, "next reduce round input count")?)
            .bind(to_i64(
                required_u64(&updated, "reduce_round")?,
                "reduce round",
            )?)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if advanced.rows_affected() != 1 {
                return Err(Error::InvalidRepositoryData {
                    message: "completed reduce round lost its cursor fence".to_string(),
                });
            }
            updated_node_status = "pending".to_string();
        }
    }
    // `waiting_reasons_truncated` is derived from the post-update counters rather than
    // maintained separately. Not every durable wait has a sampleable reason — a WaitingReplan
    // task has no `WaitingReason` variant at all — so a wait that raises `waiting_task_count`
    // without appending a sample must still leave the row readable by `run_from_row`.
    let run_updated = sqlx::query(
        "UPDATE moa.execution_run SET ready_task_count = ready_task_count + $2, \
             active_task_count = active_task_count + $3, \
             waiting_task_count = waiting_task_count + $4, \
             waiting_input_task_count = waiting_input_task_count + $5, \
             waiting_review_task_count = waiting_review_task_count + $6, \
             waiting_signal_task_count = waiting_signal_task_count + $7, \
             waiting_timer_task_count = waiting_timer_task_count + $8, \
             waiting_external_task_count = waiting_external_task_count + $9, \
             waiting_replan_task_count = waiting_replan_task_count + $10, \
             waiting_input_user_task_count = waiting_input_user_task_count + $11, \
             waiting_input_tenant_admin_task_count = \
                 waiting_input_tenant_admin_task_count + $12, \
             waiting_input_external_task_count = waiting_input_external_task_count + $13, \
             waiting_reasons_truncated = \
                 jsonb_array_length(waiting_reasons) < waiting_task_count + $4, \
             last_progress_at = GREATEST(last_progress_at, NOW()), \
             wake_epoch = wake_epoch + 1, updated_at = NOW() \
         WHERE run_uid = $1 AND ready_task_count + $2 >= 0 AND active_task_count + $3 >= 0 \
           AND waiting_task_count + $4 >= 0 AND waiting_input_task_count + $5 >= 0 \
           AND waiting_review_task_count + $6 >= 0 \
           AND waiting_signal_task_count + $7 >= 0 \
           AND waiting_timer_task_count + $8 >= 0 \
           AND waiting_external_task_count + $9 >= 0 \
           AND waiting_replan_task_count + $10 >= 0 \
           AND waiting_input_user_task_count + $11 >= 0 \
           AND waiting_input_tenant_admin_task_count + $12 >= 0 \
           AND waiting_input_external_task_count + $13 >= 0",
    )
    .bind(run_uid)
    .bind(ready_delta)
    .bind(active_delta)
    .bind(run_waiting.total)
    .bind(run_waiting.input)
    .bind(run_waiting.review)
    .bind(run_waiting.signal)
    .bind(run_waiting.timer)
    .bind(run_waiting.external)
    .bind(run_waiting.replan)
    .bind(run_waiting.input_user)
    .bind(run_waiting.input_tenant_admin)
    .bind(run_waiting.input_external)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if run_updated.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "run ready/active/waiting counters would underflow".to_string(),
        });
    }
    if !matches!(previous_node_status.as_str(), "failed" | "cancelled")
        && matches!(updated_node_status.as_str(), "failed" | "cancelled")
    {
        cancel_unmaterialized_dependents_in_tx(conn.as_mut(), &run, node_id).await?;
    }
    if !matches!(previous_node_status.as_str(), "completed" | "skipped")
        && matches!(updated_node_status.as_str(), "completed" | "skipped")
    {
        let aggregate_output = if updated_node_status == "skipped" {
            Value::Null
        } else {
            match node.map(|node| &node.operation) {
                Some(ExecutionOperation::Map { .. }) => sqlx::query_scalar::<_, Value>(
                    "SELECT COALESCE(jsonb_agg(output ORDER BY item_key), '[]'::JSONB) \
                     FROM moa.execution_task WHERE run_uid = $1 AND node_id = $2 \
                       AND status IN ('completed', 'skipped')",
                )
                .bind(run_uid)
                .bind(node_id)
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?,
                Some(ExecutionOperation::Reduce { .. }) => sqlx::query_scalar::<_, Value>(
                    "SELECT output FROM moa.execution_task \
                     WHERE run_uid = $1 AND node_id = $2 AND status = 'completed' \
                     ORDER BY created_at DESC, task_id DESC LIMIT 1",
                )
                .bind(run_uid)
                .bind(node_id)
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?,
                Some(_) | None => sqlx::query_scalar::<_, Value>(
                    "SELECT output FROM moa.execution_task \
                     WHERE run_uid = $1 AND node_id = $2 \
                       AND status IN ('completed', 'skipped') ORDER BY task_id LIMIT 1",
                )
                .bind(run_uid)
                .bind(node_id)
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?,
            }
        };
        let aggregate_bytes = moa_core::canonical_json::canonical_json_bytes(&aggregate_output)?;
        if aggregate_bytes.len()
            > usize::try_from(MAX_ACTIVATION_OUTPUT_BYTES).map_err(|_| {
                Error::InvalidRepositoryData {
                    message: "activation output byte ceiling is invalid".to_string(),
                }
            })?
        {
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "node `{node_id}` aggregate output exceeds {MAX_ACTIVATION_OUTPUT_BYTES} bytes"
                ),
            });
        }
        sqlx::query(
            "UPDATE moa.execution_node_state SET aggregate_output = $3, \
                 aggregate_output_hash = $4, aggregate_complete = TRUE, updated_at = NOW() \
             WHERE run_uid = $1 AND node_id = $2",
        )
        .bind(run_uid)
        .bind(node_id)
        .bind(&aggregate_output)
        .bind(node_output_hash(&aggregate_output)?.to_string())
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        release_node_dependencies_in_tx(conn.as_mut(), &run, node_id).await?;
    }
    Ok(())
}

async fn cancel_unmaterialized_dependents_in_tx(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    failed_node_id: &str,
) -> Result<()> {
    let mut blocked = BTreeSet::from([failed_node_id]);
    loop {
        let prior_len = blocked.len();
        for node in &run.active_plan.definition.nodes {
            if node
                .depends_on
                .iter()
                .any(|dependency| blocked.contains(dependency.as_str()))
            {
                blocked.insert(node.id.as_str());
            }
        }
        if blocked.len() == prior_len {
            break;
        }
    }
    blocked.remove(failed_node_id);
    if blocked.is_empty() {
        return Ok(());
    }
    let dependent_ids = blocked.into_iter().collect::<Vec<_>>();
    let (pending_count, cancelled_count) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT COUNT(*) FILTER (WHERE node_status='pending' AND total_task_count=0), \
                COUNT(*) FILTER (WHERE node_status='cancelled' AND total_task_count=0 \
                  AND materialization_complete AND aggregate_complete \
                  AND remaining_dependency_count=0) \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id=ANY($2::TEXT[])",
    )
    .bind(run.run_uid)
    .bind(&dependent_ids)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let expected = i64::try_from(dependent_ids.len()).map_err(|_| Error::ArithmeticOverflow {
        context: "failed dependency node count".to_string(),
    })?;
    if pending_count + cancelled_count != expected {
        return Err(Error::InvalidRepositoryData {
            message: "failed dependency cascade found a materialized or non-pending dependent"
                .to_string(),
        });
    }
    let cancelled = sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='cancelled', \
             materialization_complete=TRUE, aggregate_complete=TRUE, \
             remaining_dependency_count=0, updated_at=NOW() \
         WHERE run_uid=$1 AND node_id=ANY($2::TEXT[]) \
           AND node_status='pending' AND total_task_count=0",
    )
    .bind(run.run_uid)
    .bind(&dependent_ids)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let applied_count =
        i64::try_from(cancelled.rows_affected()).map_err(|_| Error::ArithmeticOverflow {
            context: "newly cancelled dependency node count".to_string(),
        })?;
    if applied_count != pending_count {
        return Err(Error::InvalidRepositoryData {
            message: "failed dependency cascade lost its exact pending-node set".to_string(),
        });
    }
    Ok(())
}

async fn release_node_dependencies_in_tx(
    conn: &mut PgConnection,
    run: &ExecutionRunRecord,
    completed_node_id: &str,
) -> Result<()> {
    for dependent in run.active_plan.definition.nodes.iter().filter(|node| {
        node.depends_on
            .iter()
            .any(|dependency| dependency == completed_node_id)
    }) {
        let released = sqlx::query(
            "UPDATE moa.execution_node_state \
             SET remaining_dependency_count = remaining_dependency_count - 1,updated_at=NOW() \
             WHERE run_uid=$1 AND node_id=$2 AND remaining_dependency_count>0",
        )
        .bind(run.run_uid)
        .bind(&dependent.id)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        if released.rows_affected() != 1 {
            let already_cancelled = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM moa.execution_node_state \
                 WHERE run_uid=$1 AND node_id=$2 AND node_status='cancelled' \
                   AND total_task_count=0 AND remaining_dependency_count=0 \
                   AND materialization_complete AND aggregate_complete)",
            )
            .bind(run.run_uid)
            .bind(&dependent.id)
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_error)?;
            if already_cancelled {
                continue;
            }
            return Err(Error::InvalidRepositoryData {
                message: format!(
                    "dependent node `{}` lost its dependency counter",
                    dependent.id
                ),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct TaskCounterClass {
    ready: i64,
    active: i64,
    waiting: i64,
    terminal: i64,
    succeeded: i64,
    failed: i64,
    cancelled: i64,
}

#[derive(Clone, Copy, Debug, Default)]
struct RunWaitingCounterDelta {
    total: i64,
    input: i64,
    review: i64,
    signal: i64,
    timer: i64,
    external: i64,
    replan: i64,
    input_user: i64,
    input_tenant_admin: i64,
    input_external: i64,
}

fn run_waiting_counter_delta(
    from: ExecutionTaskStatus,
    to: ExecutionTaskStatus,
    input_audience: Option<&InputAudience>,
) -> Result<RunWaitingCounterDelta> {
    let mut delta = RunWaitingCounterDelta::default();
    apply_run_waiting_counter(&mut delta, from, input_audience, -1)?;
    apply_run_waiting_counter(&mut delta, to, input_audience, 1)?;
    Ok(delta)
}

fn apply_run_waiting_counter(
    delta: &mut RunWaitingCounterDelta,
    status: ExecutionTaskStatus,
    input_audience: Option<&InputAudience>,
    direction: i64,
) -> Result<()> {
    match status {
        ExecutionTaskStatus::WaitingInput => {
            delta.total += direction;
            delta.input += direction;
            match input_audience.ok_or_else(|| Error::InvalidRepositoryInput {
                message: "WaitingInput counter transition is missing its audience".to_string(),
            })? {
                InputAudience::User => delta.input_user += direction,
                InputAudience::TenantAdmin => delta.input_tenant_admin += direction,
                InputAudience::ExternalSystem => delta.input_external += direction,
            }
        }
        ExecutionTaskStatus::WaitingReview => {
            delta.total += direction;
            delta.review += direction;
        }
        ExecutionTaskStatus::WaitingSignal => {
            delta.total += direction;
            delta.signal += direction;
        }
        ExecutionTaskStatus::WaitingTimer => {
            delta.total += direction;
            delta.timer += direction;
        }
        ExecutionTaskStatus::WaitingExternal => {
            delta.total += direction;
            delta.external += direction;
        }
        ExecutionTaskStatus::WaitingReplan => {
            delta.total += direction;
            delta.replan += direction;
        }
        ExecutionTaskStatus::Pending
        | ExecutionTaskStatus::Ready
        | ExecutionTaskStatus::Reserved
        | ExecutionTaskStatus::Dispatching
        | ExecutionTaskStatus::Running
        | ExecutionTaskStatus::Completed
        | ExecutionTaskStatus::Skipped
        | ExecutionTaskStatus::Failed
        | ExecutionTaskStatus::Cancelled
        | ExecutionTaskStatus::UnknownOutcome => {}
    }
    Ok(())
}

const fn task_counter_class(status: ExecutionTaskStatus) -> TaskCounterClass {
    let mut counts = TaskCounterClass {
        ready: 0,
        active: 0,
        waiting: 0,
        terminal: 0,
        succeeded: 0,
        failed: 0,
        cancelled: 0,
    };
    match status {
        ExecutionTaskStatus::Ready => counts.ready = 1,
        ExecutionTaskStatus::Reserved
        | ExecutionTaskStatus::Dispatching
        | ExecutionTaskStatus::Running => counts.active = 1,
        ExecutionTaskStatus::WaitingInput
        | ExecutionTaskStatus::WaitingReview
        | ExecutionTaskStatus::WaitingSignal
        | ExecutionTaskStatus::WaitingTimer
        | ExecutionTaskStatus::WaitingExternal
        | ExecutionTaskStatus::WaitingReplan => counts.waiting = 1,
        ExecutionTaskStatus::Completed => {
            counts.terminal = 1;
            counts.succeeded = 1;
        }
        ExecutionTaskStatus::Skipped => counts.terminal = 1,
        ExecutionTaskStatus::Failed | ExecutionTaskStatus::UnknownOutcome => {
            counts.terminal = 1;
            counts.failed = 1;
        }
        ExecutionTaskStatus::Cancelled => {
            counts.terminal = 1;
            counts.cancelled = 1;
        }
        ExecutionTaskStatus::Pending => {}
    }
    counts
}

fn reduce_round_from_item_key(item_key: &str) -> Result<u64> {
    let (round, batch) = item_key
        .strip_prefix('r')
        .and_then(|value| value.split_once(":b"))
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!("reduce task item key `{item_key}` is malformed"),
        })?;
    if batch.parse::<u64>().is_err() {
        return Err(Error::InvalidRepositoryData {
            message: format!("reduce task item key `{item_key}` has an invalid batch"),
        });
    }
    round
        .parse::<u64>()
        .ok()
        .filter(|round| *round > 0)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!("reduce task item key `{item_key}` has an invalid round"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_wait_trigger_identity_replays_and_fences_generation_and_kind_offline() {
        // Pins: replaying one materialization page addresses the same timer/expiry trigger,
        // while a new logical generation or a different trigger contract cannot alias it.
        let task_id = ExecutionTaskId::from_uuid(Uuid::from_u128(41));
        let timer = storage_wait_trigger_uid(task_id, 7, ExecutionTriggerKind::TaskTimer);

        assert_eq!(
            timer,
            Uuid::parse_str("42cf2d13-1c01-5eb4-baf5-193b682fb523")
                .expect("pinned storage-wait trigger UUID")
        );
        assert_eq!(
            timer,
            storage_wait_trigger_uid(task_id, 7, ExecutionTriggerKind::TaskTimer)
        );
        assert_ne!(
            timer,
            storage_wait_trigger_uid(task_id, 8, ExecutionTriggerKind::TaskTimer)
        );
        assert_ne!(
            timer,
            storage_wait_trigger_uid(task_id, 7, ExecutionTriggerKind::WaitExpiry)
        );
    }

    #[test]
    fn thousand_wait_reasons_keep_the_same_bounded_canonical_sample_offline() {
        // Pins: high-fanout timer admission never grows the hot run-row blocker sample beyond
        // 64 entries/64KiB, and insertion order cannot change the canonical retained task IDs.
        let ordered = (1_u128..=1_000).collect::<Vec<_>>();
        let mut reversed = ordered.clone();
        reversed.reverse();
        let wake_at =
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).expect("fixed timer wake timestamp");
        let forward = accumulate_timer_sample(&ordered, wake_at);
        let reverse = accumulate_timer_sample(&reversed, wake_at);
        assert_eq!(forward, reverse);
        assert_eq!(forward.len(), MAX_WAITING_REASON_SAMPLES);
        assert!(
            serde_json::to_vec(&forward)
                .expect("serialize bounded wait sample")
                .len()
                <= MAX_WAITING_REASON_SAMPLE_BYTES
        );
        let ids = forward
            .iter()
            .filter_map(waiting_reason_task_id)
            .map(ExecutionTaskId::as_uuid)
            .collect::<Vec<_>>();
        assert_eq!(ids, (1_u128..=64).map(Uuid::from_u128).collect::<Vec<_>>());
    }

    fn accumulate_timer_sample(ids: &[u128], wake_at: DateTime<Utc>) -> Vec<WaitingReason> {
        let mut sample = Vec::new();
        for id in ids {
            let reason = WaitingReason::Timer {
                task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(*id)),
                wake: ExecutionTemporalTarget::At { at: wake_at },
            };
            sample = bounded_waiting_reason_sample(&sample, &reason, MAX_WAITING_REASON_SAMPLES)
                .expect("bound waiting reason sample");
        }
        sample
    }
}
